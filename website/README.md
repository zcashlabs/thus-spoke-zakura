# Thus Spoke Zakura website

Static public site for Thus Spoke Zakura. Host this directory as the site root; it has no build step.
The application dashboard remains in `web/`.

Contributor faces are defined in the SVG near the end of `index.html`. `main.mjs` shuffles their profiles into the existing equal-size positions on each load; position never reflects commit count.

The hero scene loads three.js, GSAP and ScrollTrigger from jsDelivr at runtime (see the importmap in
`index.html`). They are progressive: if the CDN is unreachable the page renders and works in full without
them, minus the 3D blossom and the scroll choreography. `scene.mjs` also bails out when WebGL2 is absent.

Preview locally:

```sh
python3 -m http.server 8767 --bind 127.0.0.1 --directory website
```

Open `http://127.0.0.1:8767/`. Run the site smoke checks with `node --test website/tests/*.test.mjs` from the repository root. Font licenses are in `assets/fonts/`.

## Search and AI discovery

The page's title, description, social metadata, canonical URL and JSON-LD (`SoftwareApplication`, `SoftwareSourceCode`, `WebSite`, `WebPage`, `Organization`) are in the `<head>` of `index.html`. Alongside it:

| File | Purpose |
| --- | --- |
| `robots.txt` | Allows all crawlers, names AI search and training crawlers explicitly, states `Content-Signal`, points to the sitemap |
| `sitemap.xml` | The single canonical URL; bump `<lastmod>` when the page content changes |
| `llms.txt` | Summary and links for AI agents ([llmstxt.org](https://llmstxt.org/)); Google Search ignores it |
| `index.md` | Markdown version of the page, linked with `rel="alternate"` and served with a canonical `Link` header |
| `404.html` | Not-found page; Vercel serves it with a real 404 status |
| `vercel.json` | Security headers (including the Content-Security-Policy), cache rules, and content types |

**Public origin.** The site's HTTPS address is not decided yet, so every absolute site URL uses the placeholder `https://thus-spoke-zakura.example` (`.example` is reserved and never resolves). When the address is chosen, replace it everywhere in `website/` in one pass, then submit `sitemap.xml` in Google Search Console and Bing Webmaster Tools. `tests/site-origin.test.mjs` checks that every file uses the same origin, and reports a TODO while the placeholder remains. Do not use the local preview URL for these tags.

**Keep in step.**

- `index.md` must stay content-equivalent to `index.html`. Check product facts such as ports, pools and limits against `book/` before editing either one.
- Documentation links point at the published book and must appear in `index.html`, `index.md` and `llms.txt` (tested).
- After editing the inline importmap, update its `sha256-` hash in the `vercel.json` Content-Security-Policy. The test prints the expected value.
- The CSP allows scripts only from the site and `cdn.jsdelivr.net`, and images only from the site, `data:` and GitHub avatars. Adding another origin means updating the policy.
