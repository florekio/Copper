//! Embedded start page served at `copper://start`. Goes through the same
//! HTML / CSS / layout / paint pipeline as any fetched page, so it doubles
//! as a smoke test of the rendering stack.

pub const HOST_START: &str = "start";

pub fn html_for(host: &str) -> Option<&'static str> {
    match host {
        HOST_START => Some(START_HTML),
        _ => None,
    }
}

const START_HTML: &str = r##"<!DOCTYPE html>
<html>
<head>
  <title>New Tab</title>
  <style>
    body {
      background: #f5f5f7;
      color: #2a2a32;
      font-family: serif;
      margin: 0;
      padding: 96px 32px 64px 32px;
    }
    .wordmark {
      color: #b87333;
      font-size: 64px;
      font-weight: bold;
      text-align: center;
      margin: 0 0 16px 0;
    }
    .tagline {
      color: #6a6a72;
      font-size: 16px;
      text-align: center;
      margin: 0 0 56px 0;
    }
    .grid { display: flex; margin: 0 auto; }
    .card {
      flex: 1 1 200px;
      background: white;
      border: 1px solid #d4d6dc;
      padding: 18px 20px;
      margin: 8px;
    }
    .card .label {
      color: #1f1f26;
      font-size: 16px;
      font-weight: bold;
      margin: 0 0 6px 0;
    }
    .card .url {
      color: #6a6a72;
      font-size: 13px;
      margin: 0;
    }
    .card a { color: #2a55cc; }
    .footer {
      color: #9a9aa2;
      font-size: 12px;
      text-align: center;
      margin: 64px 0 0 0;
    }
  </style>
</head>
<body>
  <h1 class="wordmark">Copper</h1>
  <p class="tagline">A from-scratch Rust browser, hosted on the Zinc JS engine</p>
  <div class="grid">
    <div class="card">
      <p class="label"><a href="https://example.com">Example Domain</a></p>
      <p class="url">example.com</p>
    </div>
    <div class="card">
      <p class="label"><a href="https://duckduckgo.com">DuckDuckGo</a></p>
      <p class="url">duckduckgo.com</p>
    </div>
    <div class="card">
      <p class="label"><a href="https://en.wikipedia.org/wiki/Web_browser">Web browser</a></p>
      <p class="url">wikipedia.org</p>
    </div>
    <div class="card">
      <p class="label"><a href="https://news.ycombinator.com">Hacker News</a></p>
      <p class="url">news.ycombinator.com</p>
    </div>
  </div>
  <p class="footer">Press Cmd+L to type a URL, Cmd+T for a new tab.</p>
</body>
</html>"##;
