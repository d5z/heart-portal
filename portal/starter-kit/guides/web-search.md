# Search + fetch

1. Call `portal_web_search` with `query` (optional `count`, default 5, max 10). You get JSON: `[{title, url, snippet}, ...]`.
2. Pick URLs from the results, then call `portal_web_fetch` with `url` to pull page text (optional `max_chars`).

Use snippets for quick context; fetch when you need the full page.
