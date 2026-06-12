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
      background: #f4f1ea;
      color: #2a2a32;
      font-family: sans-serif;
      margin: 0;
      padding: 88px 40px 56px 40px;
    }
    .hero { text-align: center; margin: 0 0 44px 0; }
    .wordmark {
      color: #b87333;
      font-size: 60px;
      font-weight: bold;
      margin: 0 0 10px 0;
      letter-spacing: -1px;
    }
    .tagline {
      color: #8a8175;
      font-size: 15px;
      margin: 0;
    }
    /* Decorative omnibox hint — mirrors the real address bar. */
    .searchbar {
      display: flex;
      align-items: center;
      max-width: 560px;
      margin: 0 auto 52px auto;
      background: #ffffff;
      border: 1px solid #e0dccf;
      border-radius: 22px;
      box-shadow: 0 2px 10px rgba(40, 30, 10, 0.06);
      padding: 12px 20px;
    }
    .searchbar .glass {
      flex-shrink: 0;
      width: 16px;
      height: 16px;
      border-radius: 9px;
      border: 2px solid #b8b0a0;
      margin-right: 12px;
    }
    .searchbar .hint { color: #9a9183; font-size: 15px; margin: 0; }
    .grid {
      display: flex;
      flex-wrap: wrap;
      justify-content: center;
      max-width: 760px;
      margin: 0 auto;
      gap: 16px;
    }
    .card {
      display: flex;
      align-items: center;
      flex: 1 1 320px;
      background: #ffffff;
      border: 1px solid #e8e4d8;
      border-radius: 14px;
      box-shadow: 0 1px 4px rgba(40, 30, 10, 0.05);
      padding: 16px 18px;
    }
    .tile {
      flex-shrink: 0;
      width: 44px;
      /* vertical centering via padding — line-height:Npx is read as a
         ratio by the engine, so we size the box with padding instead. */
      padding: 9px 0;
      text-align: center;
      border-radius: 12px;
      margin-right: 14px;
      color: #ffffff;
      font-size: 22px;
      font-weight: bold;
    }
    .t-ex { background: #6b7280; }
    .t-ddg { background: #de5833; }
    .t-wiki { background: #2a2a32; }
    .t-hn { background: #ff6600; }
    .t-mdn { background: #1b1b1b; }
    .t-gh { background: #24292f; }
    .meta { display: flex; flex-direction: column; }
    .card .label {
      color: #1f1f26;
      font-size: 16px;
      font-weight: bold;
      margin: 0 0 3px 0;
    }
    .card .url { color: #8a8175; font-size: 13px; margin: 0; }
    .card a { color: #1f1f26; }
    .footer {
      color: #a8a094;
      font-size: 12px;
      text-align: center;
      margin: 52px 0 0 0;
    }
    .footer b { color: #8a8175; }
  </style>
</head>
<body>
  <div class="hero">
    <h1 class="wordmark">Copper</h1>
    <p class="tagline">A from-scratch Rust browser, hosted on the Zinc JS engine</p>
  </div>
  <div class="searchbar">
    <div class="glass"></div>
    <p class="hint">Search DuckDuckGo or type a URL — press &#8984;L</p>
  </div>
  <div class="grid">
    <div class="card">
      <div class="tile t-ddg">D</div>
      <div class="meta">
        <p class="label"><a href="https://duckduckgo.com">DuckDuckGo</a></p>
        <p class="url">duckduckgo.com</p>
      </div>
    </div>
    <div class="card">
      <div class="tile t-wiki">W</div>
      <div class="meta">
        <p class="label"><a href="https://en.wikipedia.org/wiki/Web_browser">Wikipedia</a></p>
        <p class="url">wikipedia.org</p>
      </div>
    </div>
    <div class="card">
      <div class="tile t-hn">Y</div>
      <div class="meta">
        <p class="label"><a href="https://news.ycombinator.com">Hacker News</a></p>
        <p class="url">news.ycombinator.com</p>
      </div>
    </div>
    <div class="card">
      <div class="tile t-mdn">M</div>
      <div class="meta">
        <p class="label"><a href="https://developer.mozilla.org">MDN Web Docs</a></p>
        <p class="url">developer.mozilla.org</p>
      </div>
    </div>
    <div class="card">
      <div class="tile t-gh">G</div>
      <div class="meta">
        <p class="label"><a href="https://github.com">GitHub</a></p>
        <p class="url">github.com</p>
      </div>
    </div>
    <div class="card">
      <div class="tile t-ex">E</div>
      <div class="meta">
        <p class="label"><a href="https://example.com">Example Domain</a></p>
        <p class="url">example.com</p>
      </div>
    </div>
  </div>
  <p class="footer">Press <b>&#8984;L</b> to search or type a URL &nbsp;·&nbsp; <b>&#8984;T</b> new tab &nbsp;·&nbsp; <b>&#8984;R</b> reload</p>
</body>
</html>"##;
