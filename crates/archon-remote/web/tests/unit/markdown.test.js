import test from 'node:test';
import assert from 'node:assert/strict';
import { installDom, html, Element } from '../support/dom-shim.js';
import { safeHref } from '../../src/ui/markdown-parse.js';

installDom();
const { renderMarkdown, renderInline, clampedMarkdown } = await import('../../src/ui/markdown.js');

// The rendered body without its wrapper div.
const md = (src) => html(renderMarkdown(src)).replace(/^<div class="md">/, '').replace(/<\/div>$/, '');
const EXT = 'target="_blank" rel="noopener noreferrer"';

test('empty and blank input render nothing', () => {
  for (const src of ['', '   ', '\n\n', null, undefined]) assert.equal(md(src), '');
});

test('ATX and setext headings', () => {
  assert.equal(md('# Title'), '<h1 class="md-heading">Title</h1>');
  assert.equal(md('###### six ##'), '<h6 class="md-heading">six</h6>');
  assert.equal(md('####### seven'), '<p>####### seven</p>');
  assert.equal(md('#hash'), '<p>#hash</p>');
  assert.equal(md('Title\n===\n\nSub\n---'), '<h1 class="md-heading">Title</h1><h2 class="md-heading">Sub</h2>');
  assert.equal(md('#'), '<h1 class="md-heading"></h1>');
});

test('paragraphs, soft and hard breaks', () => {
  assert.equal(md('one\n\ntwo'), '<p>one</p><p>two</p>');
  assert.equal(md('a\nb'), '<p>a\nb</p>');
  assert.equal(md('a  \nb'), '<p>a<br>b</p>');
  assert.equal(md('a \nb'), '<p>a\nb</p>');
  assert.equal(md('a\\\nb'), '<p>a<br>b</p>');
  assert.equal(md('trailing\\'), '<p>trailing\\</p>');
});

test('strong, emphasis, strikethrough and code spans', () => {
  assert.equal(md('**ABCD**'), '<p><strong>ABCD</strong></p>');
  assert.equal(md('*a* _b_ __c__'), '<p><em>a</em> <em>b</em> <strong>c</strong></p>');
  assert.equal(md('***both***'), '<p><em><strong>both</strong></em></p>');
  assert.equal(md('~~gone~~'), '<p><del>gone</del></p>');
  assert.equal(md('`a *b*` and ``x ` y``'), '<p><code class="md-code">a *b*</code> and <code class="md-code">x ` y</code></p>');
  assert.equal(md('` a `'), '<p><code class="md-code">a</code></p>');
});

test('emphasis flanking: words, snake_case, arithmetic, unclosed marks', () => {
  assert.equal(md('snake_case_name and foo_bar_'), '<p>snake_case_name and foo_bar_</p>');
  assert.equal(md('2*3*4'), '<p>2<em>3</em>4</p>');
  assert.equal(md('a * b * c'), '<p>a * b * c</p>');
  assert.equal(md('un*frigging*believable'), '<p>un<em>frigging</em>believable</p>');
  assert.equal(md('**unclosed'), '<p>**unclosed</p>');
  assert.equal(md('*one **two*'), '<p>*one *<em>two</em></p>');
  assert.equal(md('a ~single~ and ~~~three~~~'), '<p>a ~single~ and ~~~three~~~</p>');
  assert.equal(md('**not closed `code**`'), '<p>**not closed <code class="md-code">code**</code></p>');
});

test('backslash escapes and entities', () => {
  assert.equal(md('\\*not em\\* \\# \\[x\\]'), '<p>*not em* # [x]</p>');
  assert.equal(md('&amp; &lt;b&gt; &copy; &#35; &#x41; &nosuch; &#0;'), '<p>&amp; &lt;b&gt; © # A &amp;nosuch; �</p>');
  assert.equal(md('`&amp;`'), '<p><code class="md-code">&amp;amp;</code></p>');
  assert.equal(md('&ouml; &eacute; &NotEqualTilde; &CounterClockwiseContourIntegral;'), '<p>ö é \u2242\u0338 \u2233</p>');
  assert.equal(md('[x](https://x.io/f&ouml;&ouml; "f&ouml;&ouml;")'), `<p><a class="md-link" href="https://x.io/föö" title="föö" ${EXT}>x</a></p>`);
});

test('fenced and indented code, Markdown inside stays literal', () => {
  assert.equal(md('```js\nconst a = **1**;\n# not a heading\n```'),
    '<pre class="md-pre"><code class="language-js">const a = **1**;\n# not a heading</code></pre>');
  assert.equal(md('~~~\n```\ninner\n```\n~~~'), '<pre class="md-pre"><code>```\ninner\n```</code></pre>');
  assert.equal(md('````\n```\n````'), '<pre class="md-pre"><code>```</code></pre>');
  assert.equal(md('```\nunclosed *fence*'), '<pre class="md-pre"><code>unclosed *fence*</code></pre>');
  assert.equal(md('    indented\n\n      more'), '<pre class="md-pre"><code>indented\n\n  more</code></pre>');
  assert.equal(md('```<script>x</script>\n<b>\n```'), '<pre class="md-pre"><code>&lt;b&gt;</code></pre>');
  assert.equal(md('``` js"onmouseover=x\ny\n```'), '<pre class="md-pre"><code class="language-js">y</code></pre>');
  assert.equal(md('```\na\n`~~\nb\n```'), '<pre class="md-pre"><code>a\n`~~\nb</code></pre>');
  assert.equal(md('~~~\na\n~`\n~~~'), '<pre class="md-pre"><code>a\n~`</code></pre>');
});

test('block quotes, nested and lazy', () => {
  assert.equal(md('> a\n> b'), '<blockquote class="md-quote"><p>a\nb</p></blockquote>');
  assert.equal(md('> a\n>> b\nlazy'), '<blockquote class="md-quote"><p>a</p><blockquote class="md-quote"><p>b\nlazy</p></blockquote></blockquote>');
  assert.equal(md('> # h\n> - x'), '<blockquote class="md-quote"><h1 class="md-heading">h</h1><ul class="md-list"><li>x</li></ul></blockquote>');
  // Only an open paragraph continues lazily: not a heading, a fence or a rule.
  assert.equal(md('> # heading\noutside'), '<blockquote class="md-quote"><h1 class="md-heading">heading</h1></blockquote><p>outside</p>');
  assert.equal(md('> ```\n> code\noutside\n> ```'),
    '<blockquote class="md-quote"><pre class="md-pre"><code>code</code></pre></blockquote><p>outside</p><blockquote class="md-quote"><pre class="md-pre"><code></code></pre></blockquote>');
  assert.equal(md('- # heading\noutside'), '<ul class="md-list"><li><h1 class="md-heading">heading</h1></li></ul><p>outside</p>');
  assert.equal(md('- a\nlazy'), '<ul class="md-list"><li>a\nlazy</li></ul>');
  assert.equal(md('> - # heading\noutside'), '<blockquote class="md-quote"><ul class="md-list"><li><h1 class="md-heading">heading</h1></li></ul></blockquote><p>outside</p>');
  assert.equal(md('- > # heading\noutside'), '<ul class="md-list"><li><blockquote class="md-quote"><h1 class="md-heading">heading</h1></blockquote></li></ul><p>outside</p>');
  assert.equal(md('-\nnot lazy'), '<ul class="md-list"><li></li></ul><p>not lazy</p>');
  assert.equal(md('> - ```\n>   code\noutside\n>   ```'),
    '<blockquote class="md-quote"><ul class="md-list"><li><pre class="md-pre"><code>code</code></pre></li></ul></blockquote><p>outside</p><blockquote class="md-quote"><pre class="md-pre"><code></code></pre></blockquote>');
  assert.equal(md('- > ---\noutside'), '<ul class="md-list"><li><blockquote class="md-quote"><hr class="md-rule"></blockquote></li></ul><p>outside</p>');
});

test('tabs count as indentation on every line and stay literal in code', () => {
  assert.equal(md('\tfirst\n\tsecond'), '<pre class="md-pre"><code>first\nsecond</code></pre>');
  assert.equal(md('para\n\n\n\tcode'), '<p>para</p><pre class="md-pre"><code>code</code></pre>');
  assert.equal(md('```py\n\tprint(1)\n\tprint(2)\n```'), '<pre class="md-pre"><code class="language-py">\tprint(1)\n\tprint(2)</code></pre>');
  assert.equal(md('```make\nbuild:\n\techo hello\n```'), '<pre class="md-pre"><code class="language-make">build:\n\techo hello</code></pre>');
  assert.equal(md('  \tfoo\tbaz\t\tbim'), '<pre class="md-pre"><code>foo\tbaz\t\tbim</code></pre>');
  assert.equal(md('\t\tdeeper'), '<pre class="md-pre"><code>\tdeeper</code></pre>');
  assert.equal(md('  - foo\n\n\tbar'), '<ul class="md-list"><li><p>foo</p><p>bar</p></li></ul>');
});

test('tabs after list and quote markers', () => {
  assert.equal(md('1.\tfirst\n2.\tsecond'), '<ol class="md-list"><li>first</li><li>second</li></ol>');
  assert.equal(md('-\titem'), '<ul class="md-list"><li>item</li></ul>');
  assert.equal(md('>\t- item'), '<blockquote class="md-quote"><ul class="md-list"><li>item</li></ul></blockquote>');
  assert.equal(md('-\t\tcode'), '<ul class="md-list"><li><pre class="md-pre"><code>  code</code></pre></li></ul>');
});

test('a tab in a container spans to the next stop of its real column', () => {
  const code = (text) => `<pre class="md-pre"><code>${text}</code></pre>`;
  assert.equal(md('-     \tfoo\n      \tbar'), `<ul class="md-list"><li>${code('\tfoo\n\tbar')}</li></ul>`);
  assert.equal(md('-     \t@echo hello'), `<ul class="md-list"><li>${code('\t@echo hello')}</li></ul>`);
  assert.equal(md('>\t\tfoo'), `<blockquote class="md-quote">${code('  foo')}</blockquote>`);
  assert.equal(md('> \tfoo'), '<blockquote class="md-quote"><p>foo</p></blockquote>');
  assert.equal(md('>- 1. - \t x'), `<blockquote class="md-quote"><ul class="md-list"><li><ol class="md-list"><li><ul class="md-list"><li>${code(' x')}</li></ul></li></ol></li></ul></blockquote>`);
  assert.equal(md('- a\n\n  \t# h'), '<ul class="md-list"><li><p>a</p><h1 class="md-heading">h</h1></li></ul>');
  assert.equal(md('- a\n\n  \tb'), '<ul class="md-list"><li><p>a</p><p>b</p></li></ul>');
  assert.equal(md('- ```\n  \t@echo\n  ```'), `<ul class="md-list"><li>${code('\t@echo')}</li></ul>`);
  assert.equal(md('>```make\n> all:\n> \t@echo hello\n>```'),
    '<blockquote class="md-quote"><pre class="md-pre"><code class="language-make">all:\n\t@echo hello</code></pre></blockquote>');
});

test('a marker inside quoted fenced code neither closes the fence nor lets text in', () => {
  assert.equal(md('> ```markdown\n> - ```\n> example\nOutside paragraph'),
    '<blockquote class="md-quote"><pre class="md-pre"><code class="language-markdown">- ```\nexample</code></pre></blockquote><p>Outside paragraph</p>');
  assert.equal(md('> - ```\n>   - ```\n>   example\nOutside'),
    '<blockquote class="md-quote"><ul class="md-list"><li><pre class="md-pre"><code>- ```\nexample</code></pre></li></ul></blockquote><p>Outside</p>');
  assert.equal(md('> - ```\n>   code\n> ```\n> para\nlazy'),
    '<blockquote class="md-quote"><ul class="md-list"><li><pre class="md-pre"><code>code</code></pre></li></ul><pre class="md-pre"><code>para</code></pre></blockquote><p>lazy</p>');
});

test('a closing fence indented by a short tab in a container closes it', () => {
  assert.equal(md('- ```js\n  console.log(1);\n  \t```\n  Next paragraph'),
    '<ul class="md-list"><li><pre class="md-pre"><code class="language-js">console.log(1);</code></pre>Next paragraph</li></ul>');
  assert.equal(md('> ```js\n> console.log(1);\n> \t```\n> Next paragraph'),
    '<blockquote class="md-quote"><pre class="md-pre"><code class="language-js">console.log(1);</code></pre><p>Next paragraph</p></blockquote>');
  assert.equal(md('- ```\n  a\n  \t```\nlazy?'), '<ul class="md-list"><li><pre class="md-pre"><code>a</code></pre></li></ul><p>lazy?</p>');
  assert.equal(md('```\na\n\t```\nb'), '<pre class="md-pre"><code>a\n\t```\nb</code></pre>');
});

test('nested fences open and close at the real column of their container', () => {
  assert.equal(md('> - ```\n>   code\n>   \t```\n>   next\nlazy'),
    '<blockquote class="md-quote"><ul class="md-list"><li><pre class="md-pre"><code>code\n\t```\nnext</code></pre></li></ul></blockquote><p>lazy</p>');
  assert.equal(md('- > ```\n  > code\n  > \t```\n  > next\nlazy'),
    '<ul class="md-list"><li><blockquote class="md-quote"><pre class="md-pre"><code>code\n\t```\nnext</code></pre></blockquote></li></ul><p>lazy</p>');
  assert.equal(md('- >\t```\n  >\tcode\n  >\t```\noutside'),
    '<ul class="md-list"><li><blockquote class="md-quote"><pre class="md-pre"><code>code</code></pre></blockquote></li></ul><p>outside</p>');
  assert.equal(md('> - ```\n>   code\n>  \t```\n>   next\nlazy'),
    '<blockquote class="md-quote"><ul class="md-list"><li><pre class="md-pre"><code>code</code></pre>next\nlazy</li></ul></blockquote>');
  assert.equal(md('> \t> ```js\n> \t> const x = 1;\n> \t> ```\noutside'),
    '<blockquote class="md-quote"><blockquote class="md-quote"><pre class="md-pre"><code class="language-js">const x = 1;</code></pre></blockquote></blockquote><p>outside</p>');
  assert.equal(md('- \t> ```js\n  \t> \tx\n  \t> ```\noutside'),
    '<ul class="md-list"><li><blockquote class="md-quote"><pre class="md-pre"><code class="language-js">\tx</code></pre></blockquote></li></ul><p>outside</p>');
});

test('thematic breaks', () => {
  for (const src of ['---', '***', '___', '- - -', ' * * *']) assert.equal(md(src), '<hr class="md-rule">', src);
  assert.equal(md('a\n\n---\n\nb'), '<p>a</p><hr class="md-rule"><p>b</p>');
});

test('unordered and ordered lists: tight, loose, nested, start numbers', () => {
  assert.equal(md('- a\n- b'), '<ul class="md-list"><li>a</li><li>b</li></ul>');
  assert.equal(md('- a\n\n- b'), '<ul class="md-list"><li><p>a</p></li><li><p>b</p></li></ul>');
  assert.equal(md('* a\n+ b'), '<ul class="md-list"><li>a</li></ul><ul class="md-list"><li>b</li></ul>');
  assert.equal(md('3. x\n4. y'), '<ol class="md-list" start="3"><li>x</li><li>y</li></ol>');
  assert.equal(md('1) x\n2) y'), '<ol class="md-list"><li>x</li><li>y</li></ol>');
  assert.equal(md('- a\n  - b\n    - c\n- d'),
    '<ul class="md-list"><li>a<ul class="md-list"><li>b<ul class="md-list"><li>c</li></ul></li></ul></li><li>d</li></ul>');
  assert.equal(md('1. a\n   - b\n2. c'), '<ol class="md-list"><li>a<ul class="md-list"><li>b</li></ul></li><li>c</li></ol>');
  assert.equal(md('- a\n  more\nlazy'), '<ul class="md-list"><li>a\nmore\nlazy</li></ul>');
  assert.equal(md('- a\n\n  para\n- b'), '<ul class="md-list"><li><p>a</p><p>para</p></li><li><p>b</p></li></ul>');
  assert.equal(md('- ```\n  code\n  ```'), '<ul class="md-list"><li><pre class="md-pre"><code>code</code></pre></li></ul>');
  assert.equal(md('text\n- not a list? yes it is'), '<p>text</p><ul class="md-list"><li>not a list? yes it is</li></ul>');
  assert.equal(md('text\n2. not interrupting'), '<p>text\n2. not interrupting</p>');
});

test('task lists render disabled checkboxes', () => {
  assert.equal(md('- [ ] todo\n- [x] done\n- [X] also'),
    '<ul class="md-list"><li class="md-task"><input type="checkbox" class="md-check" disabled aria-label="Not done">todo</li>'
    + '<li class="md-task"><input type="checkbox" class="md-check" disabled checked aria-label="Done">done</li>'
    + '<li class="md-task"><input type="checkbox" class="md-check" disabled checked aria-label="Done">also</li></ul>');
  assert.equal(md('- [ ]'), '<ul class="md-list"><li>[ ]</li></ul>');
});

test('tables with alignment, inline cells, escaped pipes, ragged rows', () => {
  assert.equal(md('| a | b | c |\n|:--|:-:|--:|\n| 1 | **2** | `3` |\n| only |'),
    '<div class="md-scroll"><table class="md-table"><thead><tr><th class="align-left">a</th><th class="align-center">b</th><th class="align-right">c</th></tr></thead>'
    + '<tbody><tr><td class="align-left">1</td><td class="align-center"><strong>2</strong></td><td class="align-right"><code class="md-code">3</code></td></tr>'
    + '<tr><td class="align-left">only</td><td class="align-center"></td><td class="align-right"></td></tr></tbody></table></div>');
  assert.match(md('a | b\n--|--\nx \\| y | z'), /<td>x \| y<\/td><td>z<\/td>/);
  assert.match(md('intro\n| a |\n|---|\n| 1 |'), /^<p>intro<\/p><div class="md-scroll">/);
  assert.equal(md('| a |\n|---|'), '<div class="md-scroll"><table class="md-table"><thead><tr><th>a</th></tr></thead></table></div>');
  assert.equal(md('a | b\n--|'), '<p>a | b\n--|</p>');
});

test('links, titles, references, autolinks and bare URLs', () => {
  assert.equal(md('[t](https://x.io "ti")'), `<p><a class="md-link" href="https://x.io" title="ti" ${EXT}>t</a></p>`);
  assert.equal(md('[t](<https://x.io/a b>)'), '<p>t</p>');
  assert.equal(md('[**b** `c`](mailto:a@b.co)'), `<p><a class="md-link" href="mailto:a@b.co" ${EXT}><strong>b</strong> <code class="md-code">c</code></a></p>`);
  assert.equal(md('[go](#/item/abc)'), '<p>go</p>');
  assert.equal(md('[wiki](https://x.io/a_(b))'), `<p><a class="md-link" href="https://x.io/a_(b)" ${EXT}>wiki</a></p>`);
  assert.equal(md('[r]\n\n[r]: https://r.io "T"'), `<p><a class="md-link" href="https://r.io" title="T" ${EXT}>r</a></p>`);
  assert.equal(md('[text][Ref] [Ref][]\n\n[ref]: https://r.io'), `<p><a class="md-link" href="https://r.io" ${EXT}>text</a> <a class="md-link" href="https://r.io" ${EXT}>Ref</a></p>`);
  assert.equal(md('<https://a.b/c> <me@x.org>'), `<p><a class="md-link" href="https://a.b/c" ${EXT}>https://a.b/c</a> <a class="md-link" href="mailto:me@x.org" ${EXT}>me@x.org</a></p>`);
  assert.equal(md('see https://x.io/p?q=1. And (www.x.io) and https://x.io/a_(b)).'),
    `<p>see <a class="md-link" href="https://x.io/p?q=1" ${EXT}>https://x.io/p?q=1</a>. And (<a class="md-link" href="http://www.x.io" ${EXT}>www.x.io</a>) and <a class="md-link" href="https://x.io/a_(b)" ${EXT}>https://x.io/a_(b)</a>).</p>`);
  assert.equal(md('**https://x.io**'), `<p><strong><a class="md-link" href="https://x.io" ${EXT}>https://x.io</a></strong></p>`);
  assert.equal(md('[a [b](https://x.io) c](https://y.io)'), `<p>[a <a class="md-link" href="https://x.io" ${EXT}>b</a> c](<a class="md-link" href="https://y.io" ${EXT}>https://y.io</a>)</p>`);
  assert.equal(md('xhttps://nope.io'), '<p>xhttps://nope.io</p>');
});

test('anchors never nest: inner links, autolinks and images inside a link flatten', () => {
  assert.equal(md('[![alt](https://img.io/p.png)](https://outer.io)'), `<p><a class="md-link" href="https://outer.io" ${EXT}>alt</a></p>`);
  assert.equal(md('[<https://inner.io>](https://outer.io)'), `<p>[<a class="md-link" href="https://inner.io" ${EXT}>https://inner.io</a>](<a class="md-link" href="https://outer.io" ${EXT}>https://outer.io</a>)</p>`);
  assert.equal(md('![<https://inner.io>](https://img.io)'), `<p><a class="md-link" href="https://img.io" ${EXT}>https://inner.io</a></p>`);
  for (const src of ['[a [b](https://b.io)](https://a.io)', '[![x](https://i.io)](https://o.io)', '[<https://i.io>](https://o.io)', '[x <me@x.org>](https://o.io)']) {
    const links = renderMarkdown(src).query('a');
    assert.ok(links.every((a) => a.query('a').length === 0), src);
  }
});

test('images become links and are never loaded', () => {
  const out = md('![a cat](https://x.io/c.png "t") ![](https://x.io/d.png) ![x](javascript:alert(1))');
  assert.equal(out, `<p><a class="md-link" href="https://x.io/c.png" title="t" ${EXT}>a cat</a> <a class="md-link" href="https://x.io/d.png" ${EXT}>image</a> x</p>`);
  assert.doesNotMatch(out, /<img/);
});

test('raw HTML renders as literal text', () => {
  assert.equal(md('<script>alert(1)</script>'), '<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>');
  assert.equal(md('<img src=x onerror=alert(1)>'), '<p>&lt;img src=x onerror=alert(1)&gt;</p>');
  assert.equal(md('<div>\n**bold**\n</div>'), '<p>&lt;div&gt;\n<strong>bold</strong>\n&lt;/div&gt;</p>');
  assert.equal(md('<a href="javascript:alert(1)">x</a>'), '<p>&lt;a href=&quot;javascript:alert(1)&quot;&gt;x&lt;/a&gt;</p>');
  assert.equal(md('&lt;script&gt;alert(1)&lt;/script&gt; &#60;b&#62;'), '<p>&lt;script&gt;alert(1)&lt;/script&gt; &lt;b&gt;</p>');
  assert.equal(md('<!-- c --> <style>*{}</style> <svg onload=x>'), '<p>&lt;!-- c --&gt; &lt;style&gt;*{}&lt;/style&gt; &lt;svg onload=x&gt;</p>');
});

test('only http, https and mailto become hrefs', () => {
  const refused = [
    'javascript:alert(1)', 'JaVaScRiPt:alert(1)', ' javascript:alert(1)', '\tjavascript:alert(1)', 'java\nscript:alert(1)',
    'java&#x73;cript:alert(1)', '&#106;avascript:alert(1)', 'jav&#x09;ascript:alert(1)', '&#x6A;avascript&colon;alert(1)',
    'javascript&#58;alert(1)', 'data:text/html,<script>alert(1)</script>', 'DATA:text/html;base64,PHNjcmlwdD4=', 'vbscript:msgbox(1)',
    'file:///etc/passwd', 'ftp://x.io', '//evil.io/x', '/relative', 'relative/path', 'blob:https://x.io/1', 'about:blank', 'http:', 'https://', '#/inbox', '#frag',
  ];
  for (const url of refused) {
    for (const src of [`[x](${url})`, `[x](<${url}>)`, `<${url}>`, `![x](${url})`]) {
      const out = md(src);
      assert.doesNotMatch(out, /<a |href=|<img/, `${src} -> ${out}`);
    }
  }
  assert.doesNotMatch(md('[x][r]\n\n[r]: javascript:alert(1)'), /href=/);
  assert.equal(safeHref('HTTPS://X.io/a'), 'HTTPS://X.io/a');
  assert.equal(safeHref('http://x.io/\u0000a'), null);
  assert.equal(safeHref('#/inbox'), null);
  assert.equal(safeHref('#frag'), null);
  assert.equal(safeHref('#/x" onclick="1'), null);
});

test('links that are refused still show their label', () => {
  assert.equal(md('[click **me**](javascript:alert(1))'), '<p>click <strong>me</strong></p>');
});

test('no rendered element carries a style, handler, script or javascript href', () => {
  const doc = renderMarkdown([
    '# h <script>x</script>', '[a](javascript:1) <img src=x onerror=1>', '| a |\n|---|\n| <b onclick=1> |', '```\n<script>\n```', '> <iframe>', '- <svg onload=1>',
  ].join('\n\n'));
  const walk = (el) => {
    assert.ok(!['script', 'style', 'iframe', 'img', 'svg', 'object'].includes(el.tag), el.tag);
    for (const key of el.attrs.keys()) assert.ok(!/^on|^style$/.test(key), key);
    for (const value of el.attrs.values()) assert.doesNotMatch(value, /javascript:/i);
    el.children.filter((c) => c instanceof Element).forEach(walk);
  };
  walk(doc);
});

test('renderInline keeps one line of prose inline', () => {
  assert.equal(html(renderInline('Q `x`\n**b**')), '<span class="md-inline">Q <code class="md-code">x</code> <strong>b</strong></span>');
  assert.equal(html(renderInline('run `a  b`\r\n')), '<span class="md-inline">run <code class="md-code">a  b</code></span>');
});

test('clampedMarkdown folds by whole blocks and toggles', () => {
  const src = ['first **block**', 'second block', '- third\n- list'].join('\n\n');
  assert.equal(html(clampedMarkdown(src, { limit: 1000 })), html(renderMarkdown(src)));
  const folded = clampedMarkdown(src, { limit: 20 });
  const [body, toggle] = folded.children;
  assert.equal(html(body), '<div class="md"><p>first <strong>block</strong></p></div>');
  assert.equal(toggle.textContent, 'Show all');
  toggle.listeners.click();
  assert.equal(toggle.textContent, 'Show less');
  assert.equal(toggle.getAttribute('aria-expanded'), 'true');
  assert.equal(body.query('li').length, 2);
  toggle.listeners.click();
  assert.equal(toggle.textContent, 'Show all');
  assert.equal(body.query('li').length, 0);
  assert.throws(() => clampedMarkdown(src, {}), RangeError);
});

test('a first block bigger than the limit is cut to a preview that still folds', () => {
  const big = `**${'word '.repeat(200).trim()}**`;
  const out = clampedMarkdown(`${big}\n\nafter`, { limit: 50 });
  const [body, toggle] = out.children;
  assert.ok(body.textContent.length <= 51 && body.textContent.endsWith('…'), body.textContent);
  assert.equal(toggle.textContent, 'Show all');
  toggle.listeners.click();
  assert.match(html(body), /<strong>word .*word<\/strong><\/p><p>after/);
  toggle.listeners.click();
  assert.ok(body.textContent.length <= 51);
  const single = clampedMarkdown('word '.repeat(1000), { limit: 280 });
  assert.ok(single.children[0].textContent.length <= 281);
  assert.equal(single.children[1].textContent, 'Show all');
});

test('pathological input stays fast and bounded', () => {
  const nest = (n, open, close = '') => open.repeat(n) + 'x' + close.repeat(n);
  const cases = {
    stars: '*'.repeat(200_000),
    underscores: '_a '.repeat(60_000),
    nestedEmphasis: '*a '.repeat(30_000) + 'a* '.repeat(30_000),
    nestedQuotes: nest(100_000, '>'),
    nestedLists: Array.from({ length: 5000 }, (_, i) => `${' '.repeat(i * 2)}- x`).join('\n'),
    brackets: '[a'.repeat(100_000),
    linkTails: '[a]((('.repeat(30_000),
    backticks: '`a ``b '.repeat(30_000),
    bareUrls: '(www.'.repeat(40_000),
    angle: '<a:'.repeat(60_000),
    pipes: `${'|a'.repeat(50_000)}\n${'|-'.repeat(50_000)}\n${'|x'.repeat(50_000)}`,
    blanks: `- a\n${'\n'.repeat(100_000)}  b`,
    entities: '&a;'.repeat(60_000),
    mixed: '**a_b*c`d[e](f)'.repeat(10_000),
    innerSpaces: `a${' '.repeat(190_000)}b`,
    headingSpaces: `# a${' '.repeat(190_000)}x`,
    parens: `http://a.b${')'.repeat(2038)} `.repeat(90),
    delimSpaces: `a\n-${' '.repeat(190_000)}x`,
    atxSeparator: `# ${' '.repeat(190_000)}\u2028`,
    fenceSeparator: `\`\`\`${' '.repeat(190_000)}\u2028`,
    fenceTicks: `${'`'.repeat(190_000)}\u2028`,
    refSpaces: `[a]: b${' '.repeat(190_000)}x`,
    openersThenLinks: '['.repeat(119_000) + '[]()'.repeat(19_900),
    imageOpenersThenLinks: '!['.repeat(60_000) + '[a](b)'.repeat(15_000),
    softBreaks: 'a\n'.repeat(95_000),
    spacedSoftBreaks: 'a \n'.repeat(65_000),
    lazyPipes: 'a|b\n'.repeat(40_000),
    closers: '['.repeat(95_000) + ']'.repeat(95_000),
    nestedImages: '!['.repeat(10_000) + 'a' + '](x)'.repeat(10_000),
    wideTable: '|a'.repeat(10_000) + '\n' + '|-'.repeat(10_000) + '\n' + '|\n'.repeat(4000),
    tallTable: '|a'.repeat(64) + '\n' + '|-'.repeat(64) + '\n' + '|\n'.repeat(99_000),
    deepSpans: nest(3000, '*a ', ' a*').replace(/x/, ''),
    refDefs: '[a]:x\n'.repeat(33_000),
    longRefUses: `[r]: https://a.b/${'a'.repeat(100_000)}\n\n${'[r] '.repeat(24_900)}`,
  };
  const start = performance.now();
  for (const [name, src] of Object.entries(cases)) {
    const t = performance.now();
    renderMarkdown(src);
    assert.ok(performance.now() - t < 3000, `${name} took ${Math.round(performance.now() - t)} ms`);
  }
  assert.ok(performance.now() - start < 8000, 'the whole set stays quick');
});

test('one cell budget covers every table of a document', () => {
  const sparse = `${'|h'.repeat(64)}\n${'|-'.repeat(64)}\n${'|\n'.repeat(312)}\n`;
  const out = md(`${sparse.repeat(10)}|tail|\n|-|\n|kept as text|`);
  const cells = out.split(/<t[hd][ >]/).length - 1;
  assert.ok(cells <= 20_000, `${cells} cells`);
  assert.ok(cells > 19_000, `${cells} cells`);
  assert.match(out, /kept as text/);
  assert.equal(md('| a |\n|---|\n| b |'), '<div class="md-scroll"><table class="md-table"><thead><tr><th>a</th></tr></thead><tbody><tr><td>b</td></tr></tbody></table></div>');
});

test('nesting is capped: deep quotes and lists end as text, not a crash', () => {
  const out = md(`${'> '.repeat(200)}deep`);
  assert.equal(out.split('<blockquote').length - 1, 16);
  assert.match(out, /deep/);
  const lists = md(Array.from({ length: 100 }, (_, i) => `${' '.repeat(i * 2)}- x${i}`).join('\n'));
  assert.ok(lists.split('<ul').length - 1 <= 17);
});

test('tangled emphasis across links pairs inside each link and never breaks', () => {
  const src = '**[__![*__*[[]__]**]()]()**';
  assert.equal(renderMarkdown(src).textContent, renderInline(src).textContent);
  assert.equal(md('*a [b* c](https://x.io) d*'), `<p><em>a <a class="md-link" href="https://x.io" ${EXT}>b* c</a> d</em></p>`);
  assert.equal(md('**a [*b*](https://x.io)**'), `<p><strong>a <a class="md-link" href="https://x.io" ${EXT}><em>b</em></a></strong></p>`);
});

test('many emphasis runs in one paragraph all render', () => {
  const out = md(Array.from({ length: 200 }, (_, i) => `*w${i}*`).join(' '));
  assert.equal(out.split('<em>').length - 1, 200);
  assert.ok(!out.includes('*'));
});

test('input past the size cap is cut with an ellipsis', () => {
  const out = md(`${'a'.repeat(250_000)}`);
  assert.ok(out.length < 210_000);
  assert.match(out, /…<\/p>$/);
});
