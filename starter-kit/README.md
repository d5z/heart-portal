# Starter Kit

This folder is your workspace scratchpad. Change anything here; nothing in Portal depends on these paths.

## Built-in tools

See `guides/portal-ref.md` for names, parameters, and one example each.

## Extend

- Custom MCP tools: `guides/diy-tools.md` and `tools/mcp.toml`
- Combine search + fetch: `guides/web-search.md`

## Web Search

`portal_web_search` uses **Brave Search API** if `BRAVE_API_KEY` is set, otherwise falls back to DuckDuckGo HTML scraping (no key needed).

To use Brave: set `BRAVE_API_KEY` in the environment before starting Portal.

## Source Code

Portal is open source: [github.com/d5z/heart-portal](https://github.com/d5z/heart-portal)
