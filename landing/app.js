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

  function showSlide(i) {
    var list = activeSlides();
    slideIndex = ((i % list.length) + list.length) % list.length;
    var s = list[slideIndex];
    slideImg.src = s.src;
    slideCaption.textContent = (I18N[lang] || I18N.es)[s.cap] || "";
    [...thumbs.children].forEach(function (b, j) {
      b.setAttribute("aria-selected", j === slideIndex ? "true" : "false");
    });
  }

  document.getElementById("slidePrev").addEventListener("click", function () { showSlide(slideIndex - 1); });
  document.getElementById("slideNext").addEventListener("click", function () { showSlide(slideIndex + 1); });

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
