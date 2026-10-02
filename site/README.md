# Unlatch website

A static, single-page site for Unlatch, plus the agent install skill. No framework and no
build dependencies beyond `bash` and `python3` (and headless Chrome, only to redraw the share card).

```
site/
  brand.json         every user-facing name: product, npm package, commands, domain, repo, license
  benchmarks.json    every number shown on the page, with its conditions (preliminary); each
                     entry names its bench/results/SCORECARD.md metric, and the build fails
                     when a value or `×` ratio differs from that scorecard section
  src/index.html     page template (edit this)
  src/SKILL.md       agent skill template (edit this)
  src/og.html        1200x630 share card template
  build.sh           renders the templates
  og.sh              renders src/og.html to dist/og.png with headless Chrome
  dist/              generated: the deployable site
    index.html       full HTML document (doctype, meta, canonical, Open Graph + Twitter tags, favicon)
    og.png           share card (from og.sh; committed)
    SKILL.md         served as text/markdown
    llms.txt         same content, served as text/plain
    _headers         content types for Netlify / Cloudflare Pages
    vercel.json      content types for Vercel
```

## Build

```sh
./site/build.sh          # page, skill, dist/
./site/og.sh             # only after changing the name, tagline or command
```

The build fails if any `{{placeholder}}` is left unrendered, or if a number in
`benchmarks.json` no longer matches the scorecard. Commit the generated files with the
templates, so the deploy (`site/dist/`) and the npm skill never drift.

## Names

Every user-facing name on the site lives in `brand.json`; the npm installer keeps its own copy in
`npm/unlatch/lib/names.js`. `build.sh` also writes the agent skill into the npm package
(`npm/unlatch/skill/SKILL.md`) when that package's CLI name equals `brand.json`'s `npm`, and
`npm/unlatch/test/skill.test.js` checks the two copies stay identical.

## Deploy

**Register unlatch.dev first.** It is not registered yet. Until it is, the page, `brand.json`
and the skill name it as the intended home and say "coming soon". The repo
(github.com/sudhanshuheda/unlatch) is public.

`site/dist/` is a plain static folder. Any static host works:

- **Vercel**: `cd site/dist && vercel deploy --prod` (reads `vercel.json` for content types).
- **Netlify** or **Cloudflare Pages**: publish directory `site/dist`, no build command
  (or `bash site/build.sh`). `_headers` sets the content types.
- **GitHub Pages**: publish `site/dist` from a branch or an Actions workflow. Pages serves
  `.md` as `text/markdown` and `.txt` as `text/plain` already.

After deploying, check that agents get the real files and not the HTML page, and that the
share card resolves:

```sh
curl -sI https://unlatch.dev/SKILL.md | grep -i content-type   # text/markdown
curl -sI https://unlatch.dev/llms.txt | grep -i content-type   # text/plain
curl -s  https://unlatch.dev/SKILL.md | head -3                # starts with ---
curl -sI https://unlatch.dev/og.png  | grep -i content-type    # image/png
```

## Rules for editing the page

- No numbers without conditions. Add them to `benchmarks.json`, not the HTML. Never round in
  Unlatch's favour, and say so when a target is missed.
- Competitor claims link to that tool's own docs. Cells we have not checked say "not checked"
  or "not measured".
- No testimonials, logos, download counts or star counts.
- Demos are labelled "illustration · not a recording".
- Keep the "not on npm yet" and "coming soon" chips until `npx unlatch` works for a visitor.
- The page must read completely with JavaScript off and with reduced motion on.
