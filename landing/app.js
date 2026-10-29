// NextSync landing - tema, idioma, menu movil, slider con lightbox y escena de sync.
(function () {
  "use strict";

  var root = document.documentElement;
  var dict = function () { return I18N[lang] || I18N.es; };

  // ---------- Tema ----------
  function systemTheme() {
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  function currentTheme() {
    return localStorage.getItem("nextsync-theme") || systemTheme();
  }
  function applyTheme(theme) {
    root.setAttribute("data-theme", theme);
    localStorage.setItem("nextsync-theme", theme);
    var meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", theme === "dark" ? "#15161a" : "#f6f5f4");
    document.dispatchEvent(new CustomEvent("themechange", { detail: { theme: theme } }));
  }
  applyTheme(currentTheme());
  document.getElementById("themeBtn").addEventListener("click", function () {
    applyTheme(currentTheme() === "dark" ? "light" : "dark");
  });

  // ---------- Idioma ----------
  var langButtons = [].slice.call(document.querySelectorAll(".language-switch button"));
  var lang = currentLang();
  applyLang(lang);
  syncLangButtons();
  langButtons.forEach(function (btn) {
    btn.addEventListener("click", function () {
      lang = btn.getAttribute("data-lang");
      applyLang(lang);
      syncLangButtons();
    });
  });
  function syncLangButtons() {
    langButtons.forEach(function (btn) {
      var on = btn.getAttribute("data-lang") === lang;
      btn.classList.toggle("active", on);
      btn.setAttribute("aria-pressed", on ? "true" : "false");
    });
  }

  // ---------- Menu movil ----------
  var mobileMenu = document.getElementById("mobileMenu");
  mobileMenu.querySelectorAll("a").forEach(function (a) {
    a.addEventListener("click", function () { mobileMenu.open = false; });
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && mobileMenu.open) mobileMenu.open = false;
  });

  // ---------- Slider de capturas ----------
  // Three windows times two themes, in the current language.
  var WINDOWS = [
    { base: "main", cap: "shots.capMain", alt: "shots.altMain" },
    { base: "activity", cap: "shots.capActivity", alt: "shots.altActivity" },
    { base: "settings", cap: "shots.capSettings", alt: "shots.altSettings" }
  ];
  var shotImg = document.getElementById("slideImg");
  var shotCaption = document.getElementById("shotCaption");
  var shotViewport = document.getElementById("shotViewport");
  var lightbox = document.getElementById("lightbox");
  var lightboxImg = document.getElementById("lightboxImg");
  var lightboxCaption = document.getElementById("lightboxCaption");
  var windowButtons = [].slice.call(document.querySelectorAll("[data-window]"));
  var themeButtons = [].slice.call(document.querySelectorAll("[data-shot-theme]"));
  var winIndex = 0;
  var shotTheme = currentTheme();

  // The shots are served with a 30 day cache and keep their file names, so the
  // query string is what tells a returning visitor that the picture changed.
  var SHOTS_V = "?v=2";

  function path(win, theme, suffix) {
    return "assets/shots/" + WINDOWS[win].base + "-" + lang + "-" + theme + suffix + SHOTS_V;
  }
  function captionKey(win, theme) {
    return dict()[WINDOWS[win].cap + (theme === "dark" ? "Dark" : "Light")] || "";
  }
  function altKey(win) {
    return dict()[WINDOWS[win].alt] || "";
  }

  function paintSlide() {
    shotImg.srcset = [
      path(winIndex, shotTheme, "-320.webp") + " 320w",
      path(winIndex, shotTheme, "-640.webp") + " 640w",
      path(winIndex, shotTheme, "-960.webp") + " 960w",
      path(winIndex, shotTheme, "-1080.webp") + " 1080w",
      path(winIndex, shotTheme, ".webp") + " 1440w"
    ].join(", ");
    shotImg.src = path(winIndex, shotTheme, "-1080.webp");
    shotImg.alt = altKey(winIndex);
    shotCaption.textContent = captionKey(winIndex, shotTheme);
    windowButtons.forEach(function (b, i) {
      var on = i === winIndex;
      b.classList.toggle("active", on);
      b.setAttribute("aria-selected", on ? "true" : "false");
    });
    themeButtons.forEach(function (b) {
      var on = b.getAttribute("data-shot-theme") === shotTheme;
      b.classList.toggle("active", on);
      b.setAttribute("aria-selected", on ? "true" : "false");
    });
  }

  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  var animating = false;

  function mod(i, n) { return ((i % n) + n) % n; }

  // dir: +1 next, -1 previous.
  function showWindow(i, dir) {
    var target = mod(i, WINDOWS.length);
    if (dir === undefined) dir = target === winIndex ? 1 : (target > winIndex ? 1 : -1);
    if (target === winIndex || animating || reduceMotion.matches) {
      winIndex = target;
      paintSlide();
      shotImg.style.transition = "";
      shotImg.style.transform = "";
      shotImg.style.opacity = "";
      return;
    }
    animating = true;
    shotImg.style.transition = "transform 180ms ease, opacity 180ms ease";
    shotImg.style.transform = "translateX(" + (-dir * 32) + "px)";
    shotImg.style.opacity = "0.35";
    window.setTimeout(function () {
      winIndex = target;
      paintSlide();
      shotImg.style.transition = "none";
      shotImg.style.transform = "translateX(" + (dir * 32) + "px)";
      void shotImg.offsetWidth;
      shotImg.style.transition = "transform 200ms ease, opacity 200ms ease";
      shotImg.style.transform = "translateX(0)";
      shotImg.style.opacity = "1";
      window.setTimeout(function () {
        animating = false;
        shotImg.style.transition = "";
        shotImg.style.transform = "";
        shotImg.style.opacity = "";
      }, 220);
    }, 180);
  }

  windowButtons.forEach(function (b) {
    b.addEventListener("click", function () { showWindow(Number(b.getAttribute("data-window"))); });
  });
  themeButtons.forEach(function (b) {
    b.addEventListener("click", function () {
      shotTheme = b.getAttribute("data-shot-theme");
      paintSlide();
    });
  });
  document.getElementById("slidePrev").addEventListener("click", function () { showWindow(winIndex - 1, -1); });
  document.getElementById("slideNext").addEventListener("click", function () { showWindow(winIndex + 1, 1); });

  // Arrow keys move between windows when the tabs have focus.
  [].slice.call(document.querySelectorAll(".window-tabs, .theme-tabs")).forEach(function (group) {
    group.addEventListener("keydown", function (e) {
      if (["ArrowLeft", "ArrowRight", "Home", "End"].indexOf(e.key) === -1) return;
      e.preventDefault();
      if (group.classList.contains("window-tabs")) {
        var n = WINDOWS.length;
        var next = e.key === "Home" ? 0 : e.key === "End" ? n - 1 : winIndex + (e.key === "ArrowRight" ? 1 : -1);
        showWindow(next, e.key === "ArrowLeft" || e.key === "Home" ? -1 : 1);
        windowButtons[mod(next, n)].focus();
      } else {
        shotTheme = e.key === "ArrowLeft" || e.key === "Home" ? "light" : "dark";
        paintSlide();
        (shotTheme === "light" ? themeButtons[0] : themeButtons[1]).focus();
      }
    });
  });

  // ---------- Swipe (tap y arrastre) ----------
  var drag = null;
  var suppressLightbox = false;

  function resetSlideImage() {
    shotImg.style.transition = "transform 200ms ease, opacity 200ms ease";
    shotImg.style.transform = "translateX(0)";
    shotImg.style.opacity = "1";
  }

  shotViewport.addEventListener("pointerdown", function (e) {
    if (e.button !== 0 || animating) return;
    drag = { x: e.clientX, y: e.clientY, dx: 0, active: false, id: e.pointerId };
  });
  shotViewport.addEventListener("pointermove", function (e) {
    if (!drag || e.pointerId !== drag.id) return;
    var dx = e.clientX - drag.x;
    var dy = e.clientY - drag.y;
    if (!drag.active) {
      if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return;
      if (Math.abs(dy) > Math.abs(dx)) { drag = null; return; }
      drag.active = true;
      shotViewport.setPointerCapture(e.pointerId);
      shotImg.style.transition = "none";
    }
    drag.dx = dx;
    shotImg.style.transform = "translateX(" + dx + "px)";
    shotImg.style.opacity = String(Math.max(0.55, 1 - Math.abs(dx) / 600));
    e.preventDefault();
  });
  function endDrag() {
    if (!drag) return;
    var d = drag;
    drag = null;
    if (!d.active) return;
    suppressLightbox = true;
    window.setTimeout(function () { suppressLightbox = false; }, 300);
    var threshold = Math.min(70, shotViewport.clientWidth * 0.18);
    if (Math.abs(d.dx) > threshold) {
      var dir = d.dx < 0 ? 1 : -1;
      showWindow(winIndex + dir, dir);
    } else {
      resetSlideImage();
    }
  }
  shotViewport.addEventListener("pointerup", endDrag);
  shotViewport.addEventListener("pointercancel", function () {
    if (drag && drag.active) resetSlideImage();
    drag = null;
  });

  // ---------- Lightbox ----------
  shotImg.addEventListener("click", function () {
    if (suppressLightbox) { suppressLightbox = false; return; }
    lightboxImg.src = path(winIndex, shotTheme, ".webp");
    lightboxImg.alt = altKey(winIndex);
    lightboxCaption.textContent = captionKey(winIndex, shotTheme);
    lightbox.showModal();
  });
  document.getElementById("lightboxClose").addEventListener("click", function () { lightbox.close(); });
  lightbox.addEventListener("click", function (e) {
    if (e.target === lightbox) lightbox.close();
  });

  // The site theme drives the screenshot theme, keeping the window on screen.
  document.addEventListener("themechange", function (e) {
    shotTheme = e.detail.theme;
    paintSlide();
  });
  document.addEventListener("langchange", function () { paintSlide(); });
  paintSlide();

  // ---------- Escena de sincronizacion ----------
  var scene = document.getElementById("syncScene");
  var track = document.getElementById("syncTrack");
  var motionBtn = document.getElementById("motionToggle");

  function layoutScene() {
    var line = track.querySelector(".sync-lane-line");
    var start = line ? line.offsetLeft : 0;
    var travel = Math.max(24, track.clientWidth - start - 48);
    scene.style.setProperty("--travel", travel + "px");
    scene.style.setProperty("--travel-half", travel / 2 + "px");
  }
  if (window.ResizeObserver) new ResizeObserver(layoutScene).observe(track);
  layoutScene();

  var paused = reduceMotion.matches;
  function updateMotion() {
    scene.classList.toggle("is-paused", paused);
    motionBtn.classList.toggle("is-paused", paused);
    motionBtn.disabled = reduceMotion.matches;
    var d = dict();
    var text = reduceMotion.matches ? d["sync.paused"] : (paused ? d["sync.resume"] : d["sync.pause"]);
    var label = reduceMotion.matches ? d["sync.reducedLabel"] : (paused ? d["sync.resumeLabel"] : d["sync.pauseLabel"]);
    motionBtn.querySelector("span").textContent = text;
    motionBtn.setAttribute("aria-label", label);
  }
  motionBtn.addEventListener("click", function () {
    paused = !paused;
    updateMotion();
  });
  document.addEventListener("langchange", updateMotion);
  updateMotion();

  // ---------- Copiar comando ----------
  var copyBtn = document.getElementById("copyBtn");
  var copyLabel = copyBtn.querySelector("span");
  copyBtn.addEventListener("click", function () {
    var text = document.getElementById("installCmd").textContent;
    function done() {
      copyBtn.classList.add("is-copied");
      copyLabel.textContent = dict()["install.copied"];
      window.setTimeout(function () {
        copyBtn.classList.remove("is-copied");
        copyLabel.textContent = dict()["install.copy"];
      }, 2000);
    }
    if (navigator.clipboard && window.isSecureContext) {
      navigator.clipboard.writeText(text).then(done, function () { legacyCopy(text); done(); });
    } else {
      legacyCopy(text); done();
    }
  });
  function legacyCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text; ta.style.position = "fixed"; ta.style.opacity = "0";
    document.body.appendChild(ta); ta.select();
    try { document.execCommand("copy"); } catch (e) { /* sin portapapeles */ }
    document.body.removeChild(ta);
  }
})();
