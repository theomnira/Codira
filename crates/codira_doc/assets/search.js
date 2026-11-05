// Client-side search over window.CODIRA_SEARCH: [name, kind, href, summary].
(function () {
  var input = document.getElementById("search");
  var list = document.getElementById("results");
  if (!input || !list) return;
  var prefix = input.getAttribute("data-prefix") || "";
  var index = window.CODIRA_SEARCH || [];
  var selected = -1;

  function score(entry, q) {
    var name = entry[0].toLowerCase();
    var last = name.slice(name.lastIndexOf(".") + 1);
    if (last === q) return 0;
    if (last.indexOf(q) === 0) return 1;
    if (last.indexOf(q) >= 0) return 2;
    if (name.indexOf(q) >= 0) return 3;
    if (entry[3].toLowerCase().indexOf(q) >= 0) return 4;
    return -1;
  }

  function render() {
    var q = input.value.trim().toLowerCase();
    list.innerHTML = "";
    selected = -1;
    if (!q) { list.className = ""; return; }
    var hits = [];
    for (var i = 0; i < index.length; i++) {
      var s = score(index[i], q);
      if (s >= 0) hits.push([s, index[i]]);
    }
    hits.sort(function (a, b) { return a[0] - b[0] || a[1][0].length - b[1][0].length; });
    hits.slice(0, 50).forEach(function (hit) {
      var e = hit[1];
      var li = document.createElement("li");
      var a = document.createElement("a");
      a.href = prefix + e[2];
      a.textContent = e[0];
      var k = document.createElement("span");
      k.className = "k";
      k.textContent = e[1];
      a.appendChild(k);
      if (e[3]) {
        var s = document.createElement("span");
        s.className = "s";
        s.textContent = e[3];
        a.appendChild(s);
      }
      li.appendChild(a);
      list.appendChild(li);
    });
    list.className = hits.length ? "open" : "";
  }

  function move(delta) {
    var items = list.children;
    if (!items.length) return;
    if (selected >= 0) items[selected].className = "";
    selected = (selected + delta + items.length) % items.length;
    items[selected].className = "sel";
    items[selected].scrollIntoView({ block: "nearest" });
  }

  input.addEventListener("input", render);
  input.addEventListener("keydown", function (ev) {
    if (ev.key === "ArrowDown") { move(1); ev.preventDefault(); }
    else if (ev.key === "ArrowUp") { move(-1); ev.preventDefault(); }
    else if (ev.key === "Enter") {
      var items = list.children;
      var pick = items[selected >= 0 ? selected : 0];
      if (pick) window.location.href = pick.firstChild.href;
    } else if (ev.key === "Escape") { input.value = ""; render(); }
  });
  document.addEventListener("keydown", function (ev) {
    if (ev.key === "/" && document.activeElement !== input) { input.focus(); ev.preventDefault(); }
  });
})();
