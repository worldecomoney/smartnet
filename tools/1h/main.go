// 1h — run-once holders updater (see .ref/1h/1h.md).
//
// One pass over the node's `blocks/` shards rebuilds wallet balances:
//
//	A - native WEM holders  -> {DATADIR_PATH}/native.json    (created here if absent)
//	B - contract holders    -> {DATADIR_PATH}/bytecodes/**/*.json (stubbed at contract creation)
//
// Every file is the same format — header + holders — except a contract file carries
// the contract address in its header:
//
//	{"block": 1234, "address": "0x…", "holders": [{"r":1,"a":"0x…","b":"123","p":12.5}]}
//
// `block` is the last block this file accounts for; the first run replays from block 0
// and stamps the tip, later runs replay only the delta. At most --limit holders are
// written per file, most to least. With --rpc the collected balances are refreshed
// against the live node just before the sort-and-write. Writes are atomic.
//
// This output is statistical data, NOT a source of truth (operator, 2026-11-19) —
// the files are the only artifacts; there is no hidden state beyond their `block`
// cursors, and a wallet leaving the top stays out until its next transfer.
package main

import (
	"bufio"
	"bytes"
	"compress/gzip"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"log"
	"math/big"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"time"
)

// ---------------- config ----------------

// DATADIR_PATH is the node's data dir — the full path to the data this tool reads and
// writes (`blocks/` shards, `bytecodes/` holders files, `native.json`). Hard-coded by
// design; `~` is expanded to the local home at runtime.
const DATADIR_PATH = "~/.smartnet/data"

const (
	blocksPerY  = int64(10_000) // one sub-shard = 10_000 blocks (`SHARD_Z_SIZE`)
	subShardsXY = int64(100)    // sub-shards per main shard (`SHARD_Y_SIZE`)
	// Transfer(address,address,uint256)
	transferTopic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
	// balanceOf(address)
	balanceOfSelector = "0x70a08231"
)

// ---------------- file formats ----------------

// TokenHolder is one holder row — the same record everywhere (.ref/1h/1h.md).
type TokenHolder struct {
	R int     `json:"r"` // rank
	A string  `json:"a"` // address
	B string  `json:"b"` // balance (uint256 string)
	P float64 `json:"p"` // % of supply
}

// HoldersFile is the contract-file format: header keys + holders. `Address` only
// exists on contract files.
type HoldersFile struct {
	Block   int64         `json:"block"`
	Address string        `json:"address,omitempty"`
	Holders []TokenHolder `json:"holders"`
}

// nativeEntry is one native-coin row — the native file is deliberately plain
// (operator, 2026-11-19: `{"block":1234},{"address":"","amount":""},..`).
type nativeEntry struct {
	Address string `json:"address"`
	Amount  string `json:"amount"` // decimal uint256 string
}

// ---------------- shard record formats (file-tier `b` / `txs` / `logs`) ----------------

type btRecord struct {
	B  uint64 `json:"b"`
	M  string `json:"m"`  // miner / proposer — fees land here
	BF uint64 `json:"bf"` // base fee per gas (absent pre-London)
}

type txRecord struct {
	B    uint64 `json:"b"`
	From string `json:"0"`
	To   string `json:"1"`
	St   bool   `json:"st"`
	GU   uint64 `json:"gu"`  // gas used
	MFP  string `json:"mfp"` // max fee per gas (hex)
	MTF  string `json:"mtf"` // max priority fee per gas (hex, optional)
	NV   string `json:"nv"`  // native value transferred (hex wei)
}

type logRecord struct {
	A string   `json:"a"` // token contract
	T []string `json:"t"` // topics
	D string   `json:"d"` // data
}

type blockLogsRecord struct {
	B  uint64      `json:"b"`
	LG []logRecord `json:"lg"`
}

// ---------------- helpers ----------------

// expandHome re-does a leading `~` to the local home directory at runtime.
func expandHome(p string) string {
	if !strings.HasPrefix(p, "~") {
		return p
	}
	home, err := os.UserHomeDir()
	if err != nil {
		log.Fatalf("home dir: %v", err)
	}
	return filepath.Join(home, strings.TrimPrefix(p[1:], "/"))
}

// hexInt parses a `0x…` quantity into a big int (0 on garbage).
func hexInt(s string) *big.Int {
	v := new(big.Int)
	if s == "" {
		return v
	}
	s = strings.TrimPrefix(s, "0x")
	if s == "" {
		return v
	}
	v.SetString(s, 16) // ignore error: garbage -> 0
	return v
}

// addr normalizes an address to lowercase 0x-hex; empty when not an address.
func addr(s string) string {
	s = strings.ToLower(strings.TrimSpace(s))
	if len(s) != 42 || !strings.HasPrefix(s, "0x") {
		return ""
	}
	return s
}

// topicAddr extracts the address out of a 32-byte topic word.
func topicAddr(t string) string {
	t = strings.TrimPrefix(t, "0x")
	if len(t) < 40 {
		return ""
	}
	return addr("0x" + t[len(t)-40:])
}

// atomicWrite writes data to path via tmp + fsync + rename.
func atomicWrite(path string, data []byte) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	tmp := path + ".tmp"
	f, err := os.OpenFile(tmp, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, 0o644)
	if err != nil {
		return err
	}
	if _, err = f.Write(data); err != nil {
		f.Close()
		return err
	}
	if err = f.Sync(); err != nil {
		f.Close()
		return err
	}
	if err = f.Close(); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// openShard opens a shard file: `<name>.jsonl` (active) or `<name>.gz` (sealed).
func openShard(dir, name string) (io.ReadCloser, bool) {
	if f, err := os.Open(filepath.Join(dir, name+".jsonl")); err == nil {
		return f, true
	}
	f, err := os.Open(filepath.Join(dir, name+".gz"))
	if err != nil {
		return nil, false
	}
	gz, err := gzip.NewReader(f)
	if err != nil {
		f.Close()
		return nil, false
	}
	return readCloser{gz, f}, true
}

type readCloser struct {
	io.Reader
	c io.Closer
}

func (r readCloser) Close() error { return r.c.Close() }

// eachLine scans a shard stream and hands each non-empty line to fn.
func eachLine(r io.Reader, fn func([]byte)) error {
	sc := bufio.NewScanner(r)
	sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	for sc.Scan() {
		line := sc.Bytes()
		if len(line) == 0 {
			continue
		}
		fn(line)
	}
	return sc.Err()
}

// ---------------- balance state ----------------

// Balances is a plain address -> wei map with in-place add/sub.
type Balances map[string]*big.Int

func (b Balances) add(a string, v *big.Int) {
	if a == "" || v.Sign() == 0 {
		return
	}
	cur, ok := b[a]
	if !ok {
		cur = new(big.Int)
		b[a] = cur
	}
	cur.Add(cur, v)
}

func (b Balances) sub(a string, v *big.Int) {
	if a == "" || v.Sign() == 0 {
		return
	}
	cur, ok := b[a]
	if !ok {
		cur = new(big.Int)
		b[a] = cur
	}
	cur.Sub(cur, v)
}

// rankedAddr is the native shape: plain {address, amount} entries, most to least.
func (b Balances) rankedAddr(limit int) []nativeEntry {
	type row struct {
		a string
		v *big.Int
	}
	rows := make([]row, 0, len(b))
	for a, v := range b {
		if v.Sign() <= 0 {
			continue
		}
		rows = append(rows, row{a, v})
	}
	sort.Slice(rows, func(i, j int) bool {
		if rows[i].v.Cmp(rows[j].v) != 0 {
			return rows[i].v.Cmp(rows[j].v) > 0
		}
		return rows[i].a < rows[j].a
	})
	if len(rows) > limit {
		rows = rows[:limit]
	}
	out := make([]nativeEntry, 0, len(rows))
	for _, r := range rows {
		out = append(out, nativeEntry{Address: r.a, Amount: r.v.String()})
	}
	return out
}

// ranked turns the map into ranked rows, most to least, capped at limit.
func (b Balances) ranked(limit int) []TokenHolder {
	type row struct {
		a string
		v *big.Int
	}
	rows := make([]row, 0, len(b))
	total := new(big.Int)
	for a, v := range b {
		if v.Sign() <= 0 {
			continue // zero and negative debt are not holders
		}
		rows = append(rows, row{a, v})
		total.Add(total, v)
	}
	sort.Slice(rows, func(i, j int) bool {
		if rows[i].v.Cmp(rows[j].v) != 0 {
			return rows[i].v.Cmp(rows[j].v) > 0
		}
		return rows[i].a < rows[j].a // stable, deterministic on ties
	})
	if len(rows) > limit {
		rows = rows[:limit]
	}
	out := make([]TokenHolder, 0, len(rows))
	for i, r := range rows {
		p := 0.0
		if total.Sign() > 0 {
			p, _ = new(big.Float).Quo(new(big.Float).SetInt(r.v), new(big.Float).SetInt(total)).Float64()
			p *= 100
		}
		out = append(out, TokenHolder{R: i + 1, A: r.a, B: r.v.String(), P: p})
	}
	return out
}

// ---------------- files ----------------

// target is one output file: native or one contract.
type target struct {
	path    string
	address string // empty for native
	block   int64  // last accounted block (header)
	bals    Balances
}

func loadTarget(path, address string) (*target, error) {
	t := &target{path: path, address: address, bals: Balances{}}
	raw, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			// "must be created by app if absent"
			return t, t.flush(0)
		}
		return nil, err
	}
	if address == "" {
		// native: [{"block":N},{"address":"0x…","amount":"…"},…] — the header object
		// first, then one entry per wallet
		var parts []json.RawMessage
		if err := json.Unmarshal(raw, &parts); err != nil {
			log.Printf("⚠️  %s: unparseable (%v) — rebuilding from block 0", path, err)
			return t, nil
		}
		for i, part := range parts {
			if i == 0 {
				var h struct {
					Block int64 `json:"block"`
				}
				if json.Unmarshal(part, &h) == nil {
					t.block = h.Block
				}
				continue
			}
			var e nativeEntry
			if json.Unmarshal(part, &e) != nil {
				continue
			}
			v := new(big.Int)
			v.SetString(e.Amount, 10) // decimal uint256 string
			t.bals[addr(e.Address)] = v
		}
		return t, nil
	}
	var f HoldersFile
	if err := json.Unmarshal(raw, &f); err != nil {
		log.Printf("⚠️  %s: unparseable (%v) — rebuilding from block 0", path, err)
		return t, nil
	}
	t.block = f.Block
	for _, h := range f.Holders {
		v := new(big.Int)
		v.SetString(h.B, 10) // b is a decimal uint256 string
		t.bals[h.A] = v
	}
	return t, nil
}

// flush sorts and atomically writes the file at the given tip block. Native is the
// plain header+entries stream; contracts keep the ranked holders file.
func (t *target) flush(tip int64) error {
	var data []byte
	var err error
	if t.address == "" {
		rows := t.bals.rankedAddr(*limit)
		out := make([]any, 0, len(rows)+1)
		out = append(out, map[string]any{"block": tip})
		for _, r := range rows {
			out = append(out, r)
		}
		data, err = json.MarshalIndent(out, "", " ")
	} else {
		f := HoldersFile{Block: tip, Address: t.address, Holders: t.bals.ranked(*limit)}
		data, err = json.MarshalIndent(f, "", " ")
	}
	if err != nil {
		return err
	}
	return atomicWrite(t.path, append(data, '\n'))
}

// ---------------- optional RPC refresh ----------------

type rpcReq struct {
	JSONRPC string `json:"jsonrpc"`
	ID      int    `json:"id"`
	Method  string `json:"method"`
	Params  []any  `json:"params"`
}

type rpcResp struct {
	ID     int    `json:"id"`
	Result string `json:"result"`
	Error  *struct {
		Message string `json:"message"`
	} `json:"error"`
}

// refresh pulls live balances for every collected wallet: eth_getBalance for native,
// balanceOf for tokens. Any failure keeps the replayed values.
func refresh(t *target, client *http.Client, url string) {
	type job struct {
		a   string
		req rpcReq
	}
	jobs := make([]job, 0, len(t.bals))
	id := 0
	for a := range t.bals {
		id++
		params := []any{a, "latest"}
		method := "eth_getBalance"
		if t.address != "" {
			method = "eth_call"
			params = []any{
				map[string]string{"to": t.address, "data": balanceOfSelector + strings.Repeat("0", 24) + a[2:]},
				"latest",
			}
		}
		jobs = append(jobs, job{a, rpcReq{JSONRPC: "2.0", ID: id, Method: method, Params: params}})
	}
	for i := 0; i < len(jobs); i += 200 {
		end := i + 200
		if end > len(jobs) {
			end = len(jobs)
		}
		batch := make([]rpcReq, 0, end-i)
		for _, j := range jobs[i:end] {
			batch = append(batch, j.req)
		}
		body, _ := json.Marshal(batch)
		resp, err := client.Post(url, "application/json", bytes.NewReader(body))
		if err != nil {
			log.Printf("⚠️  rpc: %v — keeping replayed balances", err)
			return
		}
		var out []rpcResp
		err = json.NewDecoder(resp.Body).Decode(&out)
		resp.Body.Close()
		if err != nil {
			log.Printf("⚠️  rpc: bad batch response (%v) — keeping replayed balances", err)
			return
		}
		byID := map[int]string{}
		for _, r := range out {
			if r.Error == nil {
				byID[r.ID] = r.Result
			}
		}
		for _, j := range jobs[i:end] {
			if v, ok := byID[j.req.ID]; ok {
				t.bals[j.a] = hexInt(v)
			}
		}
	}
}

// ---------------- replay ----------------

// scanRange walks the sub-shards covering [from, ∞) in order and calls perShard per
// sub-shard directory. Shard math mirrors file-tier: S = N/1_000_000, Y = (N%1_000_000)/10_000,
// directory `blocks/{S}/{Y:02}`.
func scanRange(blocksDir string, from int64, perShard func(dir string)) {
	for s := int64(0); ; s++ {
		top := filepath.Join(blocksDir, strconv.FormatInt(s, 10))
		if fi, err := os.Stat(top); err != nil || !fi.IsDir() {
			return // no more main shards
		}
		for y := int64(0); y < subShardsXY; y++ {
			start := s*subShardsXY*blocksPerY + y*blocksPerY
			if start+blocksPerY <= from {
				continue // entirely before the cursor
			}
			dir := filepath.Join(top, fmt.Sprintf("%02d", y))
			if fi, err := os.Stat(dir); err != nil || !fi.IsDir() {
				continue
			}
			perShard(dir)
		}
	}
}

// ---------------- main ----------------

var (
	rpcURL = flag.String("rpc", "http://127.0.0.1:8545", "optional RPC endpoint for a live balance refresh")
	limit  = flag.Int("limit", 10_000, "max holders per file, most to least")
)

func main() {
	flag.Parse()
	log.SetFlags(log.Ltime)

	dataDir := expandHome(DATADIR_PATH)
	nativePath := filepath.Join(dataDir, "native.json")
	native, err := loadTarget(nativePath, "")
	if err != nil {
		log.Fatalf("native.json: %v", err)
	}

	// Every contract file already stubbed at creation time.
	tokens := map[string]*target{}
	err = filepath.WalkDir(filepath.Join(dataDir, "bytecodes"), func(path string, d os.DirEntry, err error) error {
		if err != nil || d.IsDir() || !strings.HasSuffix(d.Name(), ".json") {
			return nil
		}
		raw, err := os.ReadFile(path)
		if err != nil {
			return nil
		}
		var f HoldersFile
		if json.Unmarshal(raw, &f) != nil || f.Address == "" {
			return nil // not a holders stub
		}
		a := addr(f.Address)
		t, err := loadTarget(path, a)
		if err != nil {
			log.Printf("⚠️  %s: %v", path, err)
			return nil
		}
		tokens[a] = t
		return nil
	})
	if err != nil {
		log.Fatalf("bytecodes/: %v", err)
	}
	log.Printf("holders files: 1 native + %d contracts", len(tokens))

	// Replay from the oldest cursor up; each file applies only its own delta.
	from := native.block
	for _, t := range tokens {
		if t.block < from {
			from = t.block
		}
	}

	tip := from
	minerFee := Balances{} // per-block proposer, fees land there
	scanRange(filepath.Join(dataDir, "blocks"), from, func(dir string) {
		// block header facts first (fee beneficiary, base fee)
		baseFee := map[uint64]uint64{}
		miner := map[uint64]string{}
		if rc, ok := openShard(dir, "b"); ok {
			_ = eachLine(rc, func(line []byte) {
				var rec btRecord
				if json.Unmarshal(line, &rec) != nil {
					return
				}
				baseFee[rec.B] = rec.BF
				miner[rec.B] = addr(rec.M)
				if int64(rec.B) > tip {
					tip = int64(rec.B)
				}
			})
			rc.Close()
		}

		// native transfers + fees
		if rc, ok := openShard(dir, "txs"); ok {
			_ = eachLine(rc, func(line []byte) {
				var rec txRecord
				if json.Unmarshal(line, &rec) != nil {
					return // TxStub ("unsupported") and anything unreadable
				}
				if int64(rec.B) < native.block {
					return // already accounted for by an earlier run
				}
				from := addr(rec.From)
				if from == "" {
					return
				}
				// fee: gas used x effective gas price (min(mfp, bf + mtf)); the sender
				// pays it whatever the outcome, the proposer earns it
				eff := hexInt(rec.MFP)
				if cap := new(big.Int).Add(new(big.Int).SetUint64(baseFee[rec.B]), hexInt(rec.MTF)); cap.Cmp(eff) < 0 {
					eff = cap
				}
				fee := new(big.Int).Mul(new(big.Int).SetUint64(rec.GU), eff)
				native.bals.sub(from, fee)
				minerFee.add(miner[rec.B], fee)

				if !rec.St {
					return // failed: value never moved
				}
				v := hexInt(rec.NV)
				to := addr(rec.To)
				if v.Sign() > 0 && to != "" {
					native.bals.sub(from, v)
					native.bals.add(to, v)
				}
			})
			rc.Close()
		}

		// token transfers (logs), only for contracts that have a holders file
		if rc, ok := openShard(dir, "logs"); ok {
			_ = eachLine(rc, func(line []byte) {
				var rec blockLogsRecord
				if json.Unmarshal(line, &rec) != nil {
					return
				}
				for _, lg := range rec.LG {
					if len(lg.T) != 3 || lg.T[0] != transferTopic {
						continue
					}
					t, ok := tokens[addr(lg.A)]
					if !ok || int64(rec.B) < t.block {
						continue
					}
					v := hexInt(lg.D)
					t.bals.sub(topicAddr(lg.T[1]), v) // mint: 0x0..0 -> absent, sub is a no-op add below
					t.bals.add(topicAddr(lg.T[2]), v)
				}
			})
			rc.Close()
		}
	})

	// fees belong to the senders' native balances too — fold the per-block proposer
	// earnings in (kept aside so a half-seen shard cannot credit the wrong miner)
	for a, v := range minerFee {
		native.bals.add(a, v)
	}

	// optional live refresh, then sort + atomic write
	client := &http.Client{Timeout: 30 * time.Second}
	if *rpcURL != "" {
		log.Printf("rpc refresh against %s …", *rpcURL)
		refresh(native, client, *rpcURL)
		for _, t := range tokens {
			refresh(t, client, *rpcURL)
		}
	}

	if err := native.flush(tip); err != nil {
		log.Fatalf("native.json: %v", err)
	}
	for _, t := range tokens {
		if err := t.flush(tip); err != nil {
			log.Printf("⚠️  %s: %v", t.path, err)
		}
	}
	log.Printf("done — tip block %d, %d native holders, %d contract files", tip, len(native.bals), len(tokens))
}
