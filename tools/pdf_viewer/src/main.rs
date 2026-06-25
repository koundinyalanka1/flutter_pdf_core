use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use pdf_core::document::PdfDocument;
use serde_json::json;

const USAGE: &str =
    "usage: pdf_viewer [pdf-path] [--password <password>] [--port <port>] [--no-open]";

#[derive(Debug, Default)]
struct Config {
    path: String,
    password: String,
    port: Option<u16>,
    open_browser: bool,
}

fn main() -> Result<()> {
    let config = parse_args()?;
    let addr = format!("127.0.0.1:{}", config.port.unwrap_or(0));
    let listener = TcpListener::bind(&addr).with_context(|| format!("failed to bind {addr}"))?;
    let port = listener.local_addr()?.port();
    let token = make_token();
    let url = format!("http://127.0.0.1:{port}/?token={token}");

    println!("PDF viewer is running at {url}");
    println!("Press Ctrl+C to stop it.");

    if config.open_browser {
        if let Err(err) = open_browser(&url) {
            eprintln!("Could not open browser automatically: {err}");
        }
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(err) = handle_connection(stream, &config, &token) {
                    eprintln!("request failed: {err}");
                }
            }
            Err(err) => eprintln!("connection failed: {err}"),
        }
    }

    Ok(())
}

fn parse_args() -> Result<Config> {
    let mut config = Config {
        open_browser: true,
        ..Default::default()
    };
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--password" => {
                config.password = args.next().with_context(|| USAGE.to_owned())?;
            }
            "--port" => {
                let raw = args.next().with_context(|| USAGE.to_owned())?;
                config.port = Some(raw.parse().context("bad --port value")?);
            }
            "--no-open" => config.open_browser = false,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            flag if flag.starts_with('-') => bail!("{USAGE}"),
            path if config.path.is_empty() => config.path = path.to_owned(),
            _ => bail!("{USAGE}"),
        }
    }

    Ok(config)
}

fn make_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:x}{:x}", nanos, std::process::id())
}

fn handle_connection(mut stream: TcpStream, config: &Config, token: &str) -> Result<()> {
    let request = read_request(&mut stream)?;
    let Some(first_line) = request.lines().next() else {
        return Ok(());
    };
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");

    if method != "GET" {
        return respond_text(&mut stream, 405, "Method Not Allowed", "GET only");
    }

    let (path, query) = split_target(target);
    let params = parse_query(query);

    match path.as_str() {
        "/" | "/index.html" => respond_html(&mut stream, &viewer_html(config, token)?),
        "/api/open" => {
            require_token(&params, token)?;
            respond_json(&mut stream, &open_document_json(&params))
        }
        "/api/page" => {
            require_token(&params, token)?;
            respond_json(&mut stream, &page_text_json(&params))
        }
        "/pdf" => {
            require_token(&params, token)?;
            respond_pdf(&mut stream, &pdf_bytes(&params))
        }
        _ => respond_text(&mut stream, 404, "Not Found", "not found"),
    }
}

fn read_request(stream: &mut TcpStream) -> Result<String> {
    let mut buffer = [0u8; 8192];
    let mut request = Vec::new();

    loop {
        let n = stream.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..n]);
        if request.windows(4).any(|w| w == b"\r\n\r\n") || request.len() > 64 * 1024 {
            break;
        }
    }

    Ok(String::from_utf8_lossy(&request).into_owned())
}

fn split_target(target: &str) -> (String, &str) {
    match target.split_once('?') {
        Some((path, query)) => (percent_decode(path), query),
        None => (percent_decode(target), ""),
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(a), Some(b)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push((a << 4) | b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn require_token(params: &HashMap<String, String>, token: &str) -> Result<()> {
    if params.get("token").map(String::as_str) == Some(token) {
        Ok(())
    } else {
        bail!("invalid token")
    }
}

fn path_and_password(params: &HashMap<String, String>) -> Result<(&str, &str)> {
    let path = params
        .get("path")
        .map(String::as_str)
        .filter(|path| !path.trim().is_empty())
        .context("PDF path is required")?;
    let password = params.get("password").map(String::as_str).unwrap_or("");
    Ok((path, password))
}

fn open_document_json(params: &HashMap<String, String>) -> Result<Vec<u8>> {
    let (path, password) = path_and_password(params)?;
    let doc = open_document(path, password)?;
    let info = doc.inspect();
    let metadata = pdf_ops::metadata::read_metadata(&doc);
    let display_path = std::fs::canonicalize(path)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| path.to_owned());
    let value = json!({
        "ok": true,
        "path": display_path,
        "version": info.version,
        "encrypted": info.encrypted,
        "objectCount": info.object_count,
        "pageCount": info.page_count.unwrap_or(0),
        "metadata": metadata,
    });
    Ok(serde_json::to_vec(&value)?)
}

fn page_text_json(params: &HashMap<String, String>) -> Result<Vec<u8>> {
    let (path, password) = path_and_password(params)?;
    let page = params
        .get("page")
        .and_then(|page| page.parse::<usize>().ok())
        .filter(|page| *page >= 1)
        .unwrap_or(1);
    let doc = open_document(path, password)?;
    let text = pdf_text::extractor::extract_page_text(&doc, page - 1)?;
    Ok(serde_json::to_vec(&json!({
        "ok": true,
        "page": page,
        "text": text,
    }))?)
}

fn pdf_bytes(params: &HashMap<String, String>) -> Result<Vec<u8>> {
    let (path, password) = path_and_password(params)?;
    let doc = open_document(path, password)?;
    Ok(doc.to_bytes()?)
}

fn open_document(path: &str, password: &str) -> Result<PdfDocument> {
    if !Path::new(path).exists() {
        bail!("file does not exist: {path}");
    }
    PdfDocument::from_path_with_password(path, password)
        .with_context(|| format!("failed to open {path}"))
}

fn viewer_html(config: &Config, token: &str) -> Result<String> {
    let initial_path = serde_json::to_string(&config.path)?;
    let initial_password = serde_json::to_string(&config.password)?;
    let token = serde_json::to_string(token)?;
    Ok(format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>PDF Viewer</title>
  <style>
    :root {{
      color-scheme: light dark;
      --bg: #f7f5ef;
      --panel: #ffffff;
      --ink: #171717;
      --muted: #66635f;
      --line: #d8d2c5;
      --accent: #245b52;
      --accent-strong: #1b453f;
      --error: #9b1c1c;
      --code: #111827;
    }}
    @media (prefers-color-scheme: dark) {{
      :root {{
        --bg: #171717;
        --panel: #242424;
        --ink: #f4f1ea;
        --muted: #aaa39a;
        --line: #3b3833;
        --accent: #7ac7b7;
        --accent-strong: #a7e0d5;
        --error: #ff9a9a;
        --code: #f4f1ea;
      }}
    }}
    * {{ box-sizing: border-box; }}
    body {{
      margin: 0;
      min-height: 100vh;
      background: var(--bg);
      color: var(--ink);
      font: 14px/1.4 ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }}
    button, input {{
      font: inherit;
    }}
    .app {{
      min-height: 100vh;
      display: grid;
      grid-template-rows: auto 1fr;
    }}
    .toolbar {{
      display: grid;
      grid-template-columns: minmax(180px, 1fr) minmax(120px, 240px) auto;
      gap: 10px;
      align-items: center;
      padding: 12px;
      border-bottom: 1px solid var(--line);
      background: var(--panel);
    }}
    .toolbar input {{
      width: 100%;
      min-height: 36px;
      border: 1px solid var(--line);
      border-radius: 6px;
      padding: 7px 9px;
      color: var(--ink);
      background: transparent;
    }}
    .toolbar button,
    .pagebar button {{
      min-height: 36px;
      border: 1px solid var(--accent);
      border-radius: 6px;
      padding: 7px 12px;
      color: #ffffff;
      background: var(--accent);
      cursor: pointer;
    }}
    .toolbar button:hover,
    .pagebar button:hover {{
      background: var(--accent-strong);
    }}
    .main {{
      min-height: 0;
      display: grid;
      grid-template-columns: minmax(360px, 1fr) minmax(340px, 430px);
    }}
    .preview {{
      min-width: 0;
      min-height: 0;
      border-right: 1px solid var(--line);
      background: #2b2b2b;
    }}
    .preview iframe {{
      display: block;
      width: 100%;
      height: 100%;
      border: 0;
      background: #2b2b2b;
    }}
    .side {{
      min-width: 0;
      min-height: 0;
      display: grid;
      grid-template-rows: auto auto 1fr;
      background: var(--panel);
    }}
    .status {{
      min-height: 38px;
      padding: 10px 12px;
      border-bottom: 1px solid var(--line);
      color: var(--muted);
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }}
    .status.error {{ color: var(--error); }}
    .meta {{
      display: grid;
      grid-template-columns: 92px 1fr;
      gap: 4px 10px;
      padding: 12px;
      border-bottom: 1px solid var(--line);
    }}
    .meta dt {{
      color: var(--muted);
    }}
    .meta dd {{
      margin: 0;
      min-width: 0;
      overflow-wrap: anywhere;
    }}
    .pagebar {{
      display: grid;
      grid-template-columns: auto auto minmax(90px, 1fr);
      gap: 8px;
      align-items: center;
      padding: 10px 12px;
      border-bottom: 1px solid var(--line);
    }}
    .pagebar span {{
      color: var(--muted);
      white-space: nowrap;
    }}
    .text {{
      min-height: 0;
      margin: 0;
      padding: 14px 12px 24px;
      overflow: auto;
      color: var(--code);
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      font: 13px/1.55 ui-monospace, SFMono-Regular, Menlo, Consolas, "Liberation Mono", monospace;
    }}
    @media (max-width: 860px) {{
      .toolbar {{
        grid-template-columns: 1fr;
      }}
      .main {{
        grid-template-columns: 1fr;
        grid-template-rows: minmax(360px, 55vh) minmax(360px, 45vh);
      }}
      .preview {{
        border-right: 0;
        border-bottom: 1px solid var(--line);
      }}
    }}
  </style>
</head>
<body>
  <div class="app">
    <form class="toolbar" id="form">
      <input id="path" autocomplete="off" placeholder="PDF path">
      <input id="password" type="password" autocomplete="off" placeholder="Password">
      <button type="submit">Open</button>
    </form>
    <main class="main">
      <section class="preview">
        <iframe id="pdf" title="PDF preview"></iframe>
      </section>
      <aside class="side">
        <div id="status" class="status">Ready</div>
        <dl id="meta" class="meta"></dl>
        <div class="pagebar">
          <button id="prev" type="button">Prev</button>
          <button id="next" type="button">Next</button>
          <span id="pageLabel">Page 0 / 0</span>
        </div>
        <pre id="text" class="text"></pre>
      </aside>
    </main>
  </div>
  <script>
    const token = {token};
    const initialPath = {initial_path};
    const initialPassword = {initial_password};
    const state = {{ path: initialPath, password: initialPassword, page: 1, pageCount: 0 }};

    const $ = (id) => document.getElementById(id);
    const params = (extra) => {{
      const out = new URLSearchParams({{ token, path: state.path, password: state.password, ...extra }});
      return out.toString();
    }};
    const setStatus = (message, isError = false) => {{
      $('status').textContent = message;
      $('status').classList.toggle('error', isError);
    }};
    const setMeta = (items) => {{
      $('meta').innerHTML = '';
      for (const [label, value] of items) {{
        const dt = document.createElement('dt');
        const dd = document.createElement('dd');
        dt.textContent = label;
        dd.textContent = value || '-';
        $('meta').append(dt, dd);
      }}
    }};
    const request = async (url) => {{
      const response = await fetch(url, {{ cache: 'no-store' }});
      const data = await response.json();
      if (!data.ok) throw new Error(data.error || 'Request failed');
      return data;
    }};
    const openDocument = async () => {{
      state.path = $('path').value.trim();
      state.password = $('password').value;
      if (!state.path) return;
      setStatus('Opening...');
      $('text').textContent = '';
      try {{
        const data = await request('/api/open?' + params());
        state.page = 1;
        state.pageCount = Number(data.pageCount || 0);
        setMeta([
          ['File', data.path],
          ['Version', data.version],
          ['Pages', String(state.pageCount)],
          ['Objects', String(data.objectCount)],
          ['Encrypted', data.encrypted ? 'yes' : 'no'],
          ['Title', data.metadata?.title],
          ['Author', data.metadata?.author],
          ['Subject', data.metadata?.subject],
        ]);
        $('pdf').src = '/pdf?' + params({{ v: Date.now() }});
        setStatus(data.path);
        await loadPage(1);
      }} catch (err) {{
        state.pageCount = 0;
        $('pdf').removeAttribute('src');
        setMeta([]);
        $('pageLabel').textContent = 'Page 0 / 0';
        $('text').textContent = '';
        setStatus(err.message, true);
      }}
    }};
    const loadPage = async (page) => {{
      if (!state.path || state.pageCount < 1) return;
      state.page = Math.max(1, Math.min(page, state.pageCount));
      $('pageLabel').textContent = `Page ${{state.page}} / ${{state.pageCount}}`;
      $('text').textContent = 'Loading...';
      try {{
        const data = await request('/api/page?' + params({{ page: state.page }}));
        $('text').textContent = data.text || 'No extractable text on this page.';
      }} catch (err) {{
        $('text').textContent = '';
        setStatus(err.message, true);
      }}
    }};

    $('path').value = initialPath;
    $('password').value = initialPassword;
    $('form').addEventListener('submit', (event) => {{
      event.preventDefault();
      openDocument();
    }});
    $('prev').addEventListener('click', () => loadPage(state.page - 1));
    $('next').addEventListener('click', () => loadPage(state.page + 1));
    if (initialPath) openDocument();
  </script>
</body>
</html>
"#
    ))
}

fn respond_html(stream: &mut TcpStream, html: &str) -> Result<()> {
    respond(
        stream,
        200,
        "OK",
        "text/html; charset=utf-8",
        html.as_bytes(),
    )
}

fn respond_json(stream: &mut TcpStream, body: &Result<Vec<u8>>) -> Result<()> {
    match body {
        Ok(bytes) => respond(stream, 200, "OK", "application/json; charset=utf-8", bytes),
        Err(err) => {
            let bytes = serde_json::to_vec(&json!({
                "ok": false,
                "error": err.to_string(),
            }))?;
            respond(stream, 200, "OK", "application/json; charset=utf-8", &bytes)
        }
    }
}

fn respond_pdf(stream: &mut TcpStream, body: &Result<Vec<u8>>) -> Result<()> {
    match body {
        Ok(bytes) => respond(stream, 200, "OK", "application/pdf", bytes),
        Err(err) => respond_text(stream, 500, "Internal Server Error", &err.to_string()),
    }
}

fn respond_text(stream: &mut TcpStream, status: u16, reason: &str, message: &str) -> Result<()> {
    respond(
        stream,
        status,
        reason,
        "text/plain; charset=utf-8",
        message.as_bytes(),
    )
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    Command::new("open").arg(url).spawn()?.wait()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()?
        .wait()?;
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn open_browser(url: &str) -> Result<()> {
    Command::new("xdg-open").arg(url).spawn()?.wait()?;
    Ok(())
}
