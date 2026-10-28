// NextSync landing - tema, idioma, slider con lightbox, scrollspy y reveals.
(function () {
  "use strict";

  var root = document.documentElement;

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
  var langSelect = document.getElementById("langSelect");
  var lang = currentLang();
  langSelect.value = lang;
  applyLang(lang);
  langSelect.addEventListener("change", function () {
    lang = langSelect.value;
    applyLang(lang);
  });

  // ---------- Slider de capturas ----------
  // Dos diapositivas por idioma: tema claro y oscuro.
  var slides = {
    es: [
      { src: "assets/shots/main-es-light.webp", cap: "shots.capLight", theme: "light" },
      { src: "assets/shots/main-es-dark.webp", cap: "shots.capDark", theme: "dark" }
    ],
    en: [
      { src: "assets/shots/main-en-light.webp", cap: "shots.capLight", theme: "light" },
      { src: "assets/shots/main-en-dark.webp", cap: "shots.capDark", theme: "dark" }
    ]
  };
  var slideImg = document.getElementById("slideImg");
  var slideCaption = document.getElementById("slideCaption");
  var thumbs = document.getElementById("slideThumbs");
  var lightbox = document.getElementById("lightbox");
  var lightboxImg = document.getElementById("lightboxImg");
  var slideIndex = 0;

  function activeSlides() { return slides[lang] || slides.es; }

  function renderThumbs() {
    thumbs.innerHTML = "";
    activeSlides().forEach(function (s, i) {
      var b = document.createElement("button");
      b.setAttribute("role", "tab");
      b.setAttribute("aria-selected", i === slideIndex ? "true" : "false");
      var img = document.createElement("img");
      img.src = s.src; img.alt = ""; img.loading = "lazy";
      b.appendChild(img);
      b.addEventListener("click", function () { showSlide(i); });
      thumbs.appendChild(b);
    });
  }

  var reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
  var animating = false;

  function mod(i, n) { return ((i % n) + n) % n; }

  function setSlide(i) {
    var list = activeSlides();
    slideIndex = mod(i, list.length);
    var s = list[slideIndex];
    slideImg.src = s.src;
    slideCaption.textContent = (I18N[lang] || I18N.es)[s.cap] || "";
    [...thumbs.children].forEach(function (b, j) {
      b.setAttribute("aria-selected", j === slideIndex ? "true" : "false");
    });
  }

  // dir: +1 next, -1 previous. Without it the direction comes from the index.
  function showSlide(i, dir) {
    var list = activeSlides();
    var target = mod(i, list.length);
    if (dir === undefined) dir = target === slideIndex ? 1 : (target > slideIndex ? 1 : -1);
    if (target === slideIndex || animating || reduceMotion.matches) {
      setSlide(target);
      slideImg.style.transition = "";
      slideImg.style.transform = "";
      slideImg.style.opacity = "";
      return;
    }
    animating = true;
    slideImg.style.transition = "transform 180ms ease, opacity 180ms ease";
    slideImg.style.transform = "translateX(" + (-dir * 32) + "px)";
    slideImg.style.opacity = "0.35";
    window.setTimeout(function () {
      setSlide(target);
      slideImg.style.transition = "none";
      slideImg.style.transform = "translateX(" + (dir * 32) + "px)";
      void slideImg.offsetWidth;
      slideImg.style.transition = "transform 200ms ease, opacity 200ms ease";
      slideImg.style.transform = "translateX(0)";
      slideImg.style.opacity = "1";
      window.setTimeout(function () {
        animating = false;
        slideImg.style.transition = "";
        slideImg.style.transform = "";
        slideImg.style.opacity = "";
      }, 220);
    }, 180);
  }

  document.getElementById("slidePrev").addEventListener("click", function () { showSlide(slideIndex - 1, -1); });
  document.getElementById("slideNext").addEventListener("click", function () { showSlide(slideIndex + 1, 1); });

  // ---------- Swipe del slider (tap y arrastre) ----------
  var slideViewport = document.querySelector(".slider-viewport");
  var drag = null;
  var suppressLightbox = false;

  function resetSlideImage() {
    slideImg.style.transition = "transform 200ms ease, opacity 200ms ease";
    slideImg.style.transform = "translateX(0)";
    slideImg.style.opacity = "1";
  }

  slideViewport.addEventListener("pointerdown", function (e) {
    if (e.button !== 0 || animating) return;
    drag = { x: e.clientX, y: e.clientY, dx: 0, active: false, id: e.pointerId };
  });

  slideViewport.addEventListener("pointermove", function (e) {
    if (!drag || e.pointerId !== drag.id) return;
    var dx = e.clientX - drag.x;
    var dy = e.clientY - drag.y;
    if (!drag.active) {
      if (Math.abs(dx) < 8 && Math.abs(dy) < 8) return;
      if (Math.abs(dy) > Math.abs(dx)) { drag = null; return; }
      drag.active = true;
      slideViewport.setPointerCapture(e.pointerId);
      slideImg.style.transition = "none";
    }
    drag.dx = dx;
    slideImg.style.transform = "translateX(" + dx + "px)";
    slideImg.style.opacity = String(Math.max(0.55, 1 - Math.abs(dx) / 600));
    e.preventDefault();
  });

  function endDrag() {
    if (!drag) return;
    var d = drag;
    drag = null;
    if (!d.active) return;
    suppressLightbox = true;
    window.setTimeout(function () { suppressLightbox = false; }, 300);
    var threshold = Math.min(70, slideViewport.clientWidth * 0.18);
    if (Math.abs(d.dx) > threshold) {
      var dir = d.dx < 0 ? 1 : -1;
      showSlide(slideIndex + dir, dir);
    } else {
      resetSlideImage();
    }
  }

  slideViewport.addEventListener("pointerup", endDrag);
  slideViewport.addEventListener("pointercancel", function () {
    if (drag && drag.active) resetSlideImage();
    drag = null;
  });

  document.addEventListener("keydown", function (e) {
    if (lightbox.open) return;
    if (e.key === "ArrowLeft") showSlide(slideIndex - 1);
    if (e.key === "ArrowRight") showSlide(slideIndex + 1);
  });

  // Al cambiar de idioma o de tema, la diapositiva acompaña.
  document.addEventListener("langchange", function () {
    slideIndex = currentTheme() === "dark" ? 1 : 0;
    renderThumbs(); showSlide(slideIndex);
  });
  document.addEventListener("themechange", function (e) {
    showSlide(e.detail.theme === "dark" ? 1 : 0);
  });

  slideIndex = currentTheme() === "dark" ? 1 : 0;
  renderThumbs(); showSlide(slideIndex);

  // ---------- Lightbox ----------
  slideImg.addEventListener("click", function () {
    if (suppressLightbox) { suppressLightbox = false; return; }
    lightboxImg.src = slideImg.src;
    lightboxImg.alt = slideImg.alt;
    lightbox.showModal();
  });
  document.getElementById("lightboxClose").addEventListener("click", function () { lightbox.close(); });
  lightbox.addEventListener("click", function (e) {
    if (e.target === lightbox) lightbox.close();
  });

  // ---------- Copiar comando ----------
  var copyBtn = document.getElementById("copyBtn");
  copyBtn.addEventListener("click", function () {
    var text = document.getElementById("installCmd").textContent;
    function done() {
      copyBtn.textContent = (I18N[lang] || I18N.es)["install.copied"];
      setTimeout(function () { copyBtn.textContent = (I18N[lang] || I18N.es)["install.copy"]; }, 2000);
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

  // ---------- Reveal on scroll ----------
  var io = new IntersectionObserver(function (entries) {
    entries.forEach(function (en) {
      if (en.isIntersecting) { en.target.classList.add("in"); io.unobserve(en.target); }
    });
  }, { threshold: 0.12 });
  document.querySelectorAll(".reveal").forEach(function (el) { io.observe(el); });

  // ---------- Scrollspy ----------
  var navAnchors = [...document.querySelectorAll(".nav-links a[href^='#']")];
  var sections = navAnchors
    .map(function (a) { return document.querySelector(a.getAttribute("href")); })
    .filter(Boolean);
  var spy = new IntersectionObserver(function (entries) {
    entries.forEach(function (en) {
      if (!en.isIntersecting) return;
      navAnchors.forEach(function (a) {
        a.classList.toggle("active", a.getAttribute("href") === "#" + en.target.id);
      });
    });
  }, { rootMargin: "-40% 0px -55% 0px" });
  sections.forEach(function (s) { spy.observe(s); });
})();
