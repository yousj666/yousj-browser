/* Yousj 起始页逻辑：搜索引擎选择 + 搜索跳转 + 收藏夹渲染 */
(function () {
  "use strict";

  var ENGINES = [
    { id: "google",   name: "Google",     initial: "G", url: "https://www.google.com/search?q=" },
    { id: "bing",     name: "Bing",       initial: "B", url: "https://www.bing.com/search?q=" },
    { id: "brave",    name: "Brave",      initial: "B", url: "https://search.brave.com/search?q=" },
    { id: "baidu",    name: "百度",        initial: "百", url: "https://www.baidu.com/s?wd=" },
    { id: "duckduckgo", name: "DuckDuckGo", initial: "D", url: "https://duckduckgo.com/?q=" },
  ];
  var STORE_KEY = "yousj-ntp-engine";

  var form = document.getElementById("search-form");
  var input = document.getElementById("q");
  var engineBtn = document.getElementById("engine-btn");
  var engineInitial = document.getElementById("engine-initial");
  var menu = document.getElementById("engine-menu");

  function getEngine() {
    var id = null;
    try { id = localStorage.getItem(STORE_KEY); } catch (e) {}
    var found = ENGINES.filter(function (x) { return x.id === id; })[0];
    return found || ENGINES[0];
  }
  function setEngine(id) {
    try { localStorage.setItem(STORE_KEY, id); } catch (e) {}
    renderEngine();
  }
  function renderEngine() {
    var cur = getEngine();
    engineInitial.textContent = cur.initial;
    engineBtn.title = "当前搜索引擎：" + cur.name + "（点击切换）";
    menu.innerHTML = "";
    ENGINES.forEach(function (e) {
      var b = document.createElement("button");
      b.type = "button";
      b.setAttribute("role", "menuitem");
      if (e.id === cur.id) b.className = "active";
      var dot = document.createElement("span");
      dot.className = "dot";
      dot.textContent = e.initial;
      b.appendChild(dot);
      b.appendChild(document.createTextNode(e.name));
      b.addEventListener("click", function () {
        setEngine(e.id);
        menu.classList.add("hidden");
        input.focus();
      });
      menu.appendChild(b);
    });
  }

  engineBtn.addEventListener("click", function (ev) {
    ev.stopPropagation();
    menu.classList.toggle("hidden");
  });
  document.addEventListener("click", function (ev) {
    if (!menu.classList.contains("hidden") && !menu.contains(ev.target)) {
      menu.classList.add("hidden");
    }
  });

  function looksLikeUrl(s) {
    return /^[^\s]+\.[^\s]{2,}$/.test(s) && !/\s/.test(s);
  }

  form.addEventListener("submit", function (ev) {
    ev.preventDefault();
    var q = input.value.trim();
    if (!q) { input.focus(); return; }
    if (looksLikeUrl(q) && q.indexOf(" ") === -1) {
      var u = /^https?:\/\//i.test(q) ? q : "https://" + q;
      location.href = u;
    } else {
      location.href = getEngine().url + encodeURIComponent(q);
    }
  });

  /* ---- 收藏夹 ---- */
  function hostOf(url) {
    try { return new URL(url).hostname; } catch (e) { return url; }
  }
  function renderBookmarks() {
    var nav = document.getElementById("bookmarks");
    nav.innerHTML = "";
    (window.BOOKMARKS || []).forEach(function (bm) {
      var a = document.createElement("a");
      a.href = bm.url;
      a.title = bm.name + "\n" + bm.url;
      var tile = document.createElement("span");
      tile.className = "tile";
      var img = document.createElement("img");
      img.alt = "";
      img.loading = "lazy";
      img.src = "https://www.google.com/s2/favicons?domain=" + hostOf(bm.url) + "&sz=64";
      img.onerror = function () {
        tile.innerHTML = "";
        var letter = document.createElement("span");
        letter.className = "letter";
        letter.textContent = (bm.name || "?").charAt(0).toUpperCase();
        tile.appendChild(letter);
      };
      tile.appendChild(img);
      var label = document.createElement("span");
      label.className = "label";
      label.textContent = bm.name;
      a.appendChild(tile);
      a.appendChild(label);
      nav.appendChild(a);
    });
  }

  renderEngine();
  renderBookmarks();
  input.focus();
})();
