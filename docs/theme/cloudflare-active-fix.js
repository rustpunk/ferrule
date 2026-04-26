(function () {
    // Cloudflare Pages strips .html from URLs (/foo.html → 308 → /foo) and
    // provides no way to disable it. mdbook's toc.js compares sidebar links
    // (which contain .html) against document.location.href (which does not)
    // and bails when nothing matches, so the on-this-page sub-header TOC
    // is suppressed on every chapter except Introduction (the index.html
    // fallback in toc.js catches `/`).
    //
    // This shim runs from the body and re-marks the active sidebar link by
    // re-adding .html to location.pathname before comparison. By the time
    // toc.js's DOMContentLoaded handler fires, the active link is set and
    // the on-this-page injector proceeds normally.
    const sidebar = document.getElementById('mdbook-sidebar');
    if (!sidebar || sidebar.querySelector('.active')) return;

    let target = location.pathname;
    if (target.endsWith('/')) target += 'index.html';
    else if (!target.endsWith('.html')) target += '.html';

    for (const a of sidebar.querySelectorAll('a')) {
        if (new URL(a.href).pathname !== target) continue;
        a.classList.add('active');
        let p = a.parentElement;
        while (p) {
            if (p.tagName === 'LI' && p.classList.contains('chapter-item')) {
                p.classList.add('expanded');
            }
            p = p.parentElement;
        }
        break;
    }
})();
