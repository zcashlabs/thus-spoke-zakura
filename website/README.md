# Thus Spoke Zakura website

Static public site for Thus Spoke Zakura. The landing page has no build step;
the production build combines it with the rendered book.
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

## Production deployment

`.github/workflows/website.yml` checks and builds the website and book on pull
requests and pushes to `main`. Successful builds on `main` deploy both to Vercel:

- Website: <https://ths.zypherpunk.xyz/>
- Book, including Markdown pages and `llms.txt`: <https://ths.zypherpunk.xyz/book/>

Runs share a deployment concurrency group so production deployments do not
overlap. The workflow can also be run manually from `main`; other branches
only build. No deployment credentials are available to the pull-request job.

Configure the existing Vercel project and GitHub repository once:

1. Add `ths.zypherpunk.xyz` to the Vercel project's production domains and set
   the DNS records Vercel requests. Allow public access to production.
2. Clear the project's **Root Directory** setting. CI supplies an assembled
   directory containing `vercel.json` and `public/`, rather than the source
   `website/` directory. The checked-in configuration selects the static
   framework, skips installation and compilation, and serves `public/`.
3. Keep automatic Git deployments disabled. The repository-root and website
   `vercel.json` files set `git.deploymentEnabled` to `false`, covering both
   the new root setting and an existing project still rooted at `website/`.
   GitHub Actions owns deployments; CLI deployments continue to work.
4. In GitHub, create the `website-production` environment and restrict its
   deployment branches to `main`. To deploy automatically, leave required
   reviewers unset.
5. Add `VERCEL_ORG_ID` and `VERCEL_PROJECT_ID` as GitHub Actions variables in
   that environment (or at repository level), using the IDs from the project's
   local `.vercel/project.json`. Add `VERCEL_TOKEN` as an environment secret,
   using a token with access to that Vercel project. Enter credentials directly
   in GitHub; do not commit them.

To inspect the same public files locally, install mdBook 0.5.2 and run from the
repository root:

```sh
mdbook build
python3 tools/build_site.py
python3 tools/build_website.py
python3 -m http.server 8767 --bind 127.0.0.1 --directory site/deploy/public
```

Open `/` and `/book/`. `site/deploy/` is generated and ignored by Git. Only
public site files and the rendered book enter the deployment; tests and local
project metadata are excluded. Vercel's CLI builds this directory in CI, then
uploads the prebuilt output in one production deployment. See the official
[GitHub Actions deployment guide](https://vercel.com/kb/guide/how-can-i-use-github-actions-with-vercel)
and [Git deployment configuration](https://vercel.com/docs/project-configuration/git-configuration#turning-off-all-automatic-deployments).

## Browser checks

`tests/browser-smoke.mjs` exercises the actual page at 360, 390, 768, 1024 and 1440 px in both themes. It checks layout bounds, mobile menu focus and Escape, pointer and keyboard theme changes, saved theme, both clipboard buttons, footer navigation, reduced motion (including live preference changes), and the animation CDN fallback.

Run it against the local preview above. Playwright is an optional test tool; the site still needs no dependencies or build step. Install it outside the repository:

```sh
npm install --prefix /tmp/ths-website-browser --no-save --package-lock=false playwright
/tmp/ths-website-browser/node_modules/.bin/playwright install chromium
THS_PLAYWRIGHT_MODULE=/tmp/ths-website-browser/node_modules/playwright/index.mjs \
  node --test website/tests/browser-smoke.mjs
```

If Playwright is already installed in the project, the module override is unnecessary. `THS_CHROMIUM_EXECUTABLE` can select an existing Chromium binary, and `THS_WEBSITE_URL` can select a different preview or the public site. Animation checks need access to jsDelivr and a WebGL2-capable browser; the fallback check blocks jsDelivr deliberately.

Visually inspect each width and theme as well: scroll through the hero, walkthrough, controls, endpoints, docs and footer; check for overlapping or clipped content and horizontal page scroll. The hero stacks below 901 px, with the contributor heading above the faces. Reduced motion keeps the blossom still and presents all walkthrough steps without scroll choreography.

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

**Public origin.** The canonical HTTPS address is `https://ths.zypherpunk.xyz/`. All absolute site URLs, social preview images, structured data, crawler discovery files and the Markdown canonical header use this origin. `tests/site-origin.test.mjs` checks their consistency and enforces the chosen production domain. Serve `website/` at the domain root, make it publicly accessible without a login, then submit `https://ths.zypherpunk.xyz/sitemap.xml` in Google Search Console and Bing Webmaster Tools. Do not use the local preview URL for these tags.

**Keep in step.**

- `index.md` must stay content-equivalent to `index.html`. Check product facts such as ports, pools and limits against `book/` before editing either one.
- Documentation links point at `/book/` on the public domain and must appear in `index.html`, `index.md` and `llms.txt` (tested).
- After editing the inline importmap, update its `sha256-` hash in the `vercel.json` Content-Security-Policy. The test prints the expected value.
- The CSP allows scripts only from the site and `cdn.jsdelivr.net`, and images only from the site, `data:` and GitHub avatars. Adding another origin means updating the policy.
- The landing page CSP excludes `/book/`, where mdBook supplies its own scripts and inline initialization.
