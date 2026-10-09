/* Tallow Copy documentation — interactions.
   No framework, no library. Everything is keyboard reachable. */
(function () {
  'use strict';

  var reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  /* ── Syntax highlighting (vendored Prism) ─────────────────── */

  // The .tl scripting surface is small; give it a real grammar.
  Prism.languages.tl = {
    comment: /\/\/.*/,
    keyword: /\bTransfer\b/,
    function: /[A-Za-z_][\w]*(?=\()/,
    string: /"[^"\r\n]*"/,
    number: /\b\d+\b/,
    operator: /->/,
    punctuation: /[(),.]/
  };
  Prism.highlightAll();

  /* ── Copy-to-clipboard buttons ────────────────────────────── */

  document.querySelectorAll('.copy-btn').forEach(function (btn) {
    btn.addEventListener('click', function () {
      var block = btn.closest('.codeblock, .hero-command');
      var code = block && block.querySelector('code');
      if (!code) return;
      var label = btn.querySelector('.copy-label');
      var initialAria = btn.getAttribute('aria-label');
      navigator.clipboard.writeText(code.textContent).then(function () {
        btn.setAttribute('aria-label', 'Copied to clipboard');
        if (label) label.textContent = 'Copied';
        window.setTimeout(function () {
          btn.setAttribute('aria-label', initialAria);
          if (label) label.textContent = 'Copy';
        }, 1500);
      }).catch(function () { /* clipboard unavailable — stay silent */ });
    });
  });

  /* ── Mobile menu ──────────────────────────────────────────── */

  var menuToggle = document.getElementById('menu-toggle');
  var mobileNav = document.getElementById('mobile-nav');

  function setMenu(open) {
    mobileNav.hidden = !open;
    menuToggle.setAttribute('aria-expanded', String(open));
  }

  menuToggle.addEventListener('click', function () {
    setMenu(mobileNav.hidden);
  });

  mobileNav.querySelectorAll('a').forEach(function (a) {
    a.addEventListener('click', function () { setMenu(false); });
  });

  /* ── Install tabs ─────────────────────────────────────────── */

  var tablist = document.querySelector('[role="tablist"]');
  if (tablist) {
    var tabs = Array.prototype.slice.call(tablist.querySelectorAll('[role="tab"]'));

    function selectTab(tab, focus) {
      tabs.forEach(function (t) {
        var selected = t === tab;
        t.setAttribute('aria-selected', String(selected));
        t.tabIndex = selected ? 0 : -1;
        var panel = document.getElementById(t.getAttribute('aria-controls'));
        if (panel) panel.hidden = !selected;
      });
      if (focus) tab.focus();
    }

    tabs.forEach(function (tab) {
      tab.addEventListener('click', function () { selectTab(tab, false); });
    });

    tablist.addEventListener('keydown', function (e) {
      var i = tabs.indexOf(document.activeElement);
      if (i === -1) return;
      var next = null;
      if (e.key === 'ArrowRight') next = tabs[(i + 1) % tabs.length];
      else if (e.key === 'ArrowLeft') next = tabs[(i - 1 + tabs.length) % tabs.length];
      else if (e.key === 'Home') next = tabs[0];
      else if (e.key === 'End') next = tabs[tabs.length - 1];
      if (next) { e.preventDefault(); selectTab(next, true); }
    });
  }

  /* ── Scrollspy (header nav + desktop and mobile TOC) ───────── */

  var spyLinks = Array.prototype.slice.call(
    document.querySelectorAll('.site-nav a, .toc a, .toc-mobile a')
  );
  var spySections = Array.prototype.slice.call(
    document.querySelectorAll('main section[id]')
  ).filter(function (s) { return s.id !== 'top'; });

  function setCurrent(id) {
    spyLinks.forEach(function (a) {
      if (a.getAttribute('href') === '#' + id) a.setAttribute('aria-current', 'true');
      else a.removeAttribute('aria-current');
    });
  }

  function clearCurrent() {
    spyLinks.forEach(function (a) { a.removeAttribute('aria-current'); });
  }

  var spyTicking = false;
  function updateSpy() {
    if (spyTicking) return;
    spyTicking = true;
    window.requestAnimationFrame(function () {
      var line = window.scrollY + window.innerHeight * 0.3;
      var current = null;
      spySections.forEach(function (s) {
        if (s.getBoundingClientRect().top + window.scrollY <= line) current = s.id;
      });
      if (current) setCurrent(current);
      else clearCurrent();
      spyTicking = false;
    });
  }

  window.addEventListener('scroll', updateSpy, { passive: true });
  window.addEventListener('resize', updateSpy);
  updateSpy();

  /* ── Back to top ──────────────────────────────────────────── */

  var backTop = document.getElementById('back-to-top');
  var ticking = false;

  function updateBackTop() {
    if (ticking) return;
    ticking = true;
    window.requestAnimationFrame(function () {
      backTop.hidden = window.scrollY < 600;
      ticking = false;
    });
  }

  window.addEventListener('scroll', updateBackTop, { passive: true });
  updateBackTop();

  backTop.addEventListener('click', function () {
    window.scrollTo({ top: 0, behavior: reduceMotion ? 'auto' : 'smooth' });
  });

  /* ── Client-side search ───────────────────────────────────── */

  var searchDialog = document.getElementById('search-dialog');
  var searchInput = document.getElementById('search-input');
  var searchResults = document.getElementById('search-results');
  var searchEmpty = document.getElementById('search-empty');
  var searchToggle = document.getElementById('search-toggle');
  var searchClose = document.getElementById('search-close');
  var lastFocus = null;

  // Small index built from the DOM, so it can never drift from the page.
  var index = [];
  document.querySelectorAll('main section[id]').forEach(function (section) {
    var h2 = section.querySelector('h2');
    var title = h2 ? h2.textContent.trim() : section.id;
    index.push({
      id: section.id,
      title: title,
      parent: '',
      text: section.textContent.replace(/\s+/g, ' ').toLowerCase()
    });
    section.querySelectorAll('h3[id], article[id]').forEach(function (el) {
      var head = el.querySelector('h3');
      var name = head ? head.textContent.trim() : el.textContent.trim().split('\n')[0].trim();
      index.push({
        id: el.id,
        title: name,
        parent: title,
        text: el.textContent.replace(/\s+/g, ' ').toLowerCase()
      });
    });
  });

  function markedText(text, query, at) {
    // Returns a text node (or text + <mark>) for a case-insensitive hit.
    var frag = document.createDocumentFragment();
    if (at === -1) {
      frag.appendChild(document.createTextNode(text));
      return frag;
    }
    frag.appendChild(document.createTextNode(text.slice(0, at)));
    var mark = document.createElement('mark');
    mark.textContent = text.slice(at, at + query.length);
    frag.appendChild(mark);
    frag.appendChild(document.createTextNode(text.slice(at + query.length)));
    return frag;
  }

  function snippet(text, query, at) {
    if (at === -1) return document.createDocumentFragment();
    var start = Math.max(0, at - 45);
    var end = Math.min(text.length, at + query.length + 55);
    var frag = document.createDocumentFragment();
    if (start > 0) frag.appendChild(document.createTextNode('…'));
    frag.appendChild(markedText(text.slice(start, end), query, at - start));
    if (end < text.length) frag.appendChild(document.createTextNode('…'));
    return frag;
  }

  function renderResults(raw) {
    var q = raw.toLowerCase().trim();
    searchResults.innerHTML = '';
    if (!q) {
      searchEmpty.hidden = true;
      return;
    }
    var hits = [];
    index.forEach(function (entry) {
      var ti = entry.title.toLowerCase().indexOf(q);
      var xi = entry.text.indexOf(q);
      if (ti === -1 && xi === -1) return;
      hits.push({ entry: entry, ti: ti, xi: xi });
    });
    hits.sort(function (a, b) {
      return (a.ti === -1 ? 1 : 0) - (b.ti === -1 ? 1 : 0);
    });

    hits.slice(0, 12).forEach(function (hit) {
      var li = document.createElement('li');
      var a = document.createElement('a');
      a.href = '#' + hit.entry.id;
      a.className = 'result';

      var t = document.createElement('span');
      t.className = 'result-title';
      t.appendChild(markedText(hit.entry.title, q, hit.ti));
      a.appendChild(t);

      if (hit.entry.parent) {
        var s = document.createElement('span');
        s.className = 'result-section';
        s.textContent = hit.entry.parent;
        a.appendChild(s);
      }

      var snip = document.createElement('span');
      snip.className = 'result-snippet';
      snip.appendChild(snippet(hit.entry.text, q, hit.xi));
      a.appendChild(snip);

      li.appendChild(a);
      searchResults.appendChild(li);
    });

    searchEmpty.hidden = hits.length > 0;
  }

  function openSearch() {
    lastFocus = document.activeElement;
    searchDialog.hidden = false;
    searchToggle.setAttribute('aria-expanded', 'true');
    document.body.style.overflow = 'hidden';
    searchInput.value = '';
    renderResults('');
    searchInput.focus();
  }

  function closeSearch() {
    searchDialog.hidden = true;
    searchToggle.setAttribute('aria-expanded', 'false');
    document.body.style.overflow = '';
    if (lastFocus && document.contains(lastFocus)) lastFocus.focus();
  }

  searchToggle.addEventListener('click', function () {
    searchDialog.hidden ? openSearch() : closeSearch();
  });

  searchClose.addEventListener('click', closeSearch);

  searchDialog.addEventListener('click', function (e) {
    if (e.target === searchDialog) closeSearch();
  });

  searchResults.addEventListener('click', function (e) {
    if (e.target.closest('a.result')) closeSearch();
  });

  searchInput.addEventListener('input', function () {
    renderResults(searchInput.value);
  });

  searchInput.addEventListener('keydown', function (e) {
    var links = Array.prototype.slice.call(searchResults.querySelectorAll('a.result'));
    var i = links.indexOf(document.activeElement);
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      var next = i === -1 ? links[0] : links[i + 1] || links[0];
      if (next) next.focus();
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      if (i === 0) searchInput.focus();
      else if (i > 0) links[i - 1].focus();
    }
  });

  // Keep Tab inside the dialog.
  searchDialog.addEventListener('keydown', function (e) {
    if (e.key === 'Escape') { closeSearch(); return; }
    if (e.key !== 'Tab') return;
    var focusables = Array.prototype.slice.call(
      searchDialog.querySelectorAll('input, button, a[href]')
    ).filter(function (el) { return el.offsetParent !== null; });
    if (!focusables.length) return;
    var first = focusables[0];
    var last = focusables[focusables.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  });

  function isTyping(el) {
    if (!el) return false;
    var tag = el.tagName;
    return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || el.isContentEditable;
  }

  document.addEventListener('keydown', function (e) {
    var ctrlK = (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k';
    if ((e.key === '/' || ctrlK) && searchDialog.hidden && !isTyping(e.target)) {
      e.preventDefault();
      openSearch();
    }
  });
})();
