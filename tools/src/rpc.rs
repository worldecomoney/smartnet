//! Minimal hand-rolled HTTP/1.1 JSON-RPC client (keep-alive, one connection per
//! thread) — no async runtime, no TLS: local load-test RPC only.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::str::FromStr;
use std::time::Duration;

pub struct Http {
    host: String,
    port: u16,
    path: String,
    stream: Option<TcpStream>,
    buf: Vec<u8>,
    id: u64,
    timeout: Duration,
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

impl Http {
    pub fn connect(url: &str) -> Result<Self, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("rpc url must start with http:// (got {url:?})"))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|e| format!("port: {e}"))?),
            None => (authority.to_string(), 80),
        };
        let mut http = Http {
            host,
            port,
            path: path.to_string(),
            stream: None,
            buf: Vec::with_capacity(8 * 1024),
            id: 0,
            timeout: Duration::from_secs(10),
        };
        http.dial()?;
        Ok(http)
    }

    fn dial(&mut self) -> Result<(), String> {
        let s = TcpStream::connect((self.host.as_str(), self.port))
            .map_err(|e| format!("connect: {e}"))?;
        s.set_read_timeout(Some(self.timeout)).ok();
        s.set_write_timeout(Some(self.timeout)).ok();
        s.set_nodelay(true).ok();
        self.stream = Some(s);
        self.buf.clear();
        Ok(())
    }

    /// JSON-RPC call. `params` is pre-rendered JSON (e.g. `["0x..","pending"]`).
    /// Transport failures trigger exactly one reconnect + retry.
    pub fn call(&mut self, method: &str, params: &str) -> Result<Value, String> {
        self.id += 1;
        let body = format!(
            r#"{{"jsonrpc":"2.0","id":{},"method":"{}","params":{}}}"#,
            self.id, method, params
        );
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: {h}:{p}\r\nContent-Type: application/json\r\n\
             Content-Length: {len}\r\nConnection: keep-alive\r\n\r\n{body}",
            path = self.path,
            h = self.host,
            p = self.port,
            len = body.len(),
        );
        if std::env::var_os("TXSEND_DUMP").is_some() {
            eprintln!("RPC-BODY: {body}");
        }
        match self.roundtrip(req.as_bytes()) {
            Err(e) if e.starts_with("transport:") => {
                self.dial()?;
                self.roundtrip(req.as_bytes())
            }
            other => other,
        }
    }

    fn roundtrip(&mut self, req: &[u8]) -> Result<Value, String> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| "transport: not connected".to_string())?;
        stream.write_all(req).map_err(|e| format!("transport: write: {e}"))?;
        stream.flush().ok();

        let hdr_end = loop {
            if let Some(p) = find_headers_end(&self.buf) {
                break p;
            }
            let mut chunk = [0u8; 64 * 1024];
            let n = stream.read(&mut chunk).map_err(|e| format!("transport: read: {e}"))?;
            if n == 0 {
                return Err("transport: eof before headers".into());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        };
        let headers = String::from_utf8_lossy(&self.buf[..hdr_end]).into_owned();
        if !(headers.starts_with("HTTP/1.1 200") || headers.starts_with("HTTP/1.0 200")) {
            let line = headers.lines().next().unwrap_or("").to_string();
            return Err(format!("http status: {line}"));
        }
        let mut content_len = None;
        for l in headers.lines().skip(1) {
            if let Some((k, v)) = l.split_once(':') {
                if k.eq_ignore_ascii_case("content-length") {
                    content_len = v.trim().parse::<usize>().ok();
                }
            }
        }
        let clen = content_len.ok_or_else(|| "transport: no content-length".to_string())?;
        let body_start = hdr_end + 4;
        while self.buf.len() < body_start + clen {
            let mut chunk = [0u8; 64 * 1024];
            let n = stream.read(&mut chunk).map_err(|e| format!("transport: read: {e}"))?;
            if n == 0 {
                return Err("transport: eof in body".into());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
        let body = self.buf[body_start..body_start + clen].to_vec();
        self.buf.drain(..body_start + clen);

        let v: Value = serde_json::from_slice(&body).map_err(|e| format!("parse: {e}"))?;
        if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
            return Err(format!("[{code}] {msg}"));
        }
        match v.get("result") {
            Some(r) => Ok(r.clone()),
            None => Err("response has neither result nor error".into()),
        }
    }

    // ---- typed helpers -----------------------------------------------------

    pub fn nonce(&mut self, addr: &str) -> Result<u64, String> {
        let params = format!(r#"["{addr}","pending"]"#);
        let v = self.call("eth_getTransactionCount", &params)?;
        let s = v.as_str().ok_or("nonce: expected string")?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("nonce: {e}"))
    }

    pub fn balance(&mut self, addr: &str) -> Result<alloy_primitives::U256, String> {
        let v = self.call("eth_getBalance", &format!(r#"["{addr}","latest"]"#))?;
        crate::tx::parse_u256_hex(&v)
    }

    pub fn block_number(&mut self) -> Result<u64, String> {
        let v = self.call("eth_blockNumber", "[]")?;
        let s = v.as_str().ok_or("blockNumber: expected string")?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("block: {e}"))
    }

    /// `eth_gasPrice` — used by `--gas-price auto` (chainkit parity).
    pub fn gas_price(&mut self) -> Result<u128, String> {
        let v = self.call("eth_gasPrice", "[]")?;
        let s = v.as_str().ok_or("gasPrice: expected string")?;
        u128::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("gasPrice: {e}"))
    }

    /// `eth_getBlockByNumber "latest"` → baseFeePerGas (chainkit `status` parity).
    pub fn base_fee(&mut self) -> Result<u128, String> {
        let v = self.call("eth_getBlockByNumber", r#"["latest", false]"#)?;
        match v.get("baseFeePerGas").and_then(Value::as_str) {
            Some(s) => {
                u128::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("basefee: {e}"))
            }
            None => Err("no baseFeePerGas".into()),
        }
    }

    /// `eth_estimateGas` for a call (chainkit `--gas-limit 0` = estimate once, reuse).
    pub fn estimate_gas(
        &mut self,
        from: &str,
        to: Option<&str>,
        value_wei: u128,
        data_hex: Option<&str>,
    ) -> Result<u64, String> {
        let mut p = format!(r#"{{"from":"{from}""#);
        if let Some(t) = to {
            p.push_str(&format!(r#", "to":"{t}""#));
        }
        if value_wei > 0 {
            p.push_str(&format!(r#", "value":"{value_wei:#x}""#));
        }
        if let Some(d) = data_hex {
            p.push_str(&format!(r#", "data":"{d}""#));
        }
        p.push('}');
        let v = self.call("eth_estimateGas", &format!("[{p}]"))?;
        let s = v.as_str().ok_or("estimateGas: expected string")?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|e| format!("estimateGas: {e}"))
    }

    /// Raw `txpool_status` (chainkit `status` parity; may be method-not-found).
    pub fn txpool_status(&mut self) -> Result<Value, String> {
        self.call("txpool_status", "[]")
    }

    pub fn send_raw(&mut self, raw_hex: &str) -> Result<alloy_primitives::B256, String> {
        let v = self.call("eth_sendRawTransaction", &format!(r#"["{raw_hex}"]"#))?;
        let s = v.as_str().ok_or("send: expected tx hash")?;
        alloy_primitives::B256::from_str(s).map_err(|e| format!("send: {e}"))
    }

    pub fn receipt(&mut self, hash: &alloy_primitives::B256) -> Result<Option<Value>, String> {
        let v = self.call("eth_getTransactionReceipt", &format!(r#"["{hash}"]"#))?;
        Ok(if v.is_null() { None } else { Some(v) })
    }

    pub fn eth_call(&mut self, to: &str, data_hex: &str) -> Result<Value, String> {
        // params: [{"to":..,"data":..},"latest"]
        let params = format!(r#"[{{"to":"{to}","data":"{data_hex}"}},"latest"]"#);
        self.call("eth_call", &params)
    }
}

/// Classified server-side rejection for the single ordered loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcErrClass {
    /// Pool at capacity — transient: wait and retry the SAME tx (never skip a nonce).
    Full,
    /// Tx already in the pool — treat as accepted.
    Known,
    /// Our nonce is behind state — refetch pending nonce and continue.
    TooLow,
    /// Real rejection — stop the stream (skipping would create a permanent gap).
    Fatal,
}

/// Map a JSON-RPC error string to a loop-handling class.
pub fn classify_rpc_error(msg: &str) -> RpcErrClass {
    if msg.contains("txpool is full") {
        RpcErrClass::Full
    } else if msg.contains("already known") {
        RpcErrClass::Known
    } else if msg.contains("nonce too low") {
        RpcErrClass::TooLow
    } else {
        RpcErrClass::Fatal
    }
}
