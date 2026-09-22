# Authoring guide — Super Ferret research reports

All four pages share one design system. Do not write your own CSS, do not
change colours, do not add a font. Build the page from the classes below so the
set reads as one document.

## Page skeleton

Write a COMPLETE HTML document. Insert the shared stylesheet by literally
copying the contents of `assets/style.html` into the `<head>` — the pages must
be self-contained and portable, so do NOT `<link>` to a local CSS file.

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Your Page Name</title>
<!-- >>> paste the entire contents of assets/style.html here <<< -->
</head>
<body>
<a class="skip" href="#main">Skip to content</a>

<header class="topbar">
  <div class="mark">Super<span>&nbsp;Ferret</span></div>
  <nav>
    <a href="index.html">Overview</a>
    <a href="landscape.html">Landscape</a>
    <a href="indexing.html">Index structures</a>
    <a href="architecture.html">Architecture</a>
  </nav>
</header>

<div class="shell">
  <aside class="toc">
    <h2>Contents</h2>
    <ol>
      <li><a href="#s1">Section name</a></li>
      <li><a href="#s2">Section name</a>
        <ol class="sub"><li><a href="#s2a">Sub</a></li></ol>
      </li>
    </ol>
  </aside>

  <main id="main" class="numbered">
    <div class="masthead">
      <p class="eyebrow">Super Ferret · Research</p>
      <h1>Page Title</h1>
      <p class="standfirst">One or two sentences saying what this page
      establishes and for whom.</p>
      <div class="meta">
        <span><b>Compiled</b> 2026-09-04</span>
        <span><b>Scope</b> …</span>
        <span><b>Sources</b> 42 cited</span>
      </div>
    </div>

    <!-- sections -->

    <div class="sources">
      <h2 class="nonum">Sources</h2>
      <ol> <li id="r1"><a href="…">Title — publisher</a></li> </ol>
    </div>
    <footer class="pagefoot">
      <span>Super Ferret research · page N of 4</span>
      <span>Compiled 2026-09-04</span>
    </footer>
  </main>
</div>

<script>
// TOC scroll-spy. Copy verbatim.
(function () {
  var links = [].slice.call(document.querySelectorAll('.toc a[href^="#"]'));
  if (!links.length) return;
  var map = {};
  links.forEach(function (a) {
    var el = document.getElementById(a.getAttribute('href').slice(1));
    if (el) map[el.id] = a;
  });
  var obs = new IntersectionObserver(function (entries) {
    entries.forEach(function (e) {
      if (e.isIntersecting) {
        links.forEach(function (l) { l.classList.remove('on'); });
        if (map[e.target.id]) map[e.target.id].classList.add('on');
      }
    });
  }, { rootMargin: '-72px 0px -70% 0px', threshold: 0 });
  Object.keys(map).forEach(function (id) {
    var el = document.getElementById(id); if (el) obs.observe(el);
  });
})();
</script>
</body>
</html>
```

Set `aria-current="page"` on this page's own nav link.
Give every `<h2>` an `id` and list it in the TOC.

## The classes

**Sections.** `main.numbered` auto-numbers `h2`/`h3`. Add `class="nonum"` to a
heading that should not take a number (Sources, appendices).

**Provenance chips — the signature device of this report set.** Every number
carries a chip saying how it was sourced. Never state a figure without one.

```html
466 MB <span class="p p-doc">upstream</span>
3.5× source size <span class="p p-bench">benchmark</span>
~15% of corpus <span class="p p-vendor">vendor</span>
0.9 bits/posting <span class="p p-paper">paper</span>
2.1 GB/s <span class="p p-measured">measured</span>
~75 MB/M files <span class="p p-community">anecdote</span>
≈ 40 h <span class="p p-est">estimate</span>
no figure published <span class="p p-none">unknown</span>
```

Put a legend near the top of any page that uses them:

```html
<div class="legend">
  <b>Provenance</b>
  <span><span class="p p-doc">upstream</span> project docs or source</span>
  <span><span class="p p-paper">paper</span> peer-reviewed</span>
  <span><span class="p p-bench">benchmark</span> independent</span>
  <span><span class="p p-vendor">vendor</span> vendor-claimed</span>
  <span><span class="p p-measured">measured</span> measured for this report</span>
  <span><span class="p p-community">anecdote</span> community-reported</span>
  <span><span class="p p-est">estimate</span> derived arithmetic</span>
  <span><span class="p p-none">unknown</span> no figure exists</span>
</div>
```

**Tables.** Always wrap: `<div class="tw"><table>…</table></div>`. Use
`<th>` for the row's name column, `class="num"` for numeric cells,
`class="mono"` for identifiers. Add a `<caption>` where the table needs one.

**Density bars.** Index size is the report's central axis — show it, don't just
state it. Width is the ratio as a percentage of the track, capped at 100.

```html
<div class="bar">
  <span class="track"><span class="fill" style="width:20%"></span></span>
  <span class="val">0.20×</span>
</div>
```
`.fill.hi` (amber) above ~1×, `.fill.vhi` (red) above ~3×.

**Callouts.** `<div class="box key">`, `.box warn`, `.box gap` (a hole in the
public record), `.box verdict`. Each opens with
`<span class="label">Key finding</span>` or similar.

**Stat cards.**
```html
<div class="grid">
  <div class="card">
    <div class="stat">3.5<small>×</small></div>
    <div class="cap">zoekt index vs source</div>
  </div>
</div>
```

**Verdict pills.** `<span class="pill yes">yes</span>`, `.pill no`,
`.pill part`, `.pill dead` — for capability and maintenance-status columns.

**Code.** `<figure class="code"><figcaption>…</figcaption><pre><code>…</code></pre></figure>`.
Escape `<`, `>`, `&`. Inside `<pre>` you may use `<span class="cm">` for
comments and `<span class="kw">` for keywords; nothing else.

## House style

- Dave wrote Ferret, the Ruby port of Lucene. Write at peer level. No
  introductions to posting lists, BM25, or segment merging.
- Density before latency. When comparing designs, lead with bytes.
- Never invent a number. If the research file says no figure exists, say so
  with a `p-none` chip — that absence is itself a finding.
- Cite inline with a superscript link to the Sources list:
  `<sup><a href="#r7">7</a></sup>`.
- No emoji anywhere.
