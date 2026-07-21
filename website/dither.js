/* ─────────────────────────────────────────────────────────────
   goq.sh — hero dither field + install copy button.
   Vanilla, no dependencies. Ordered (Bayer 8x8) dithering of a
   slowly drifting earth-tone gradient, rendered at low resolution
   and scaled up (CSS image-rendering: pixelated) into chunky pixels.
   The pointer dents a damped wave field on the same grid, so moving
   the mouse leaves a rippling wake dithered into the terrain.
   ───────────────────────────────────────────────────────────── */
(function () {
  "use strict";

  /* ── Two natural ramps (dark → light). A low-frequency region
        noise dithers between a warm-earth ramp and a green-tan ramp,
        so the field reads like natural terrain rather than one hue.
        Both share the pale highlight so patches blend at the light end. */
  function readPalette() {
    var s = getComputedStyle(document.documentElement);
    var pick = function (name, fallback) {
      var v = s.getPropertyValue(name).trim();
      return v || fallback;
    };
    // Gentle green lean applied to the dither colours only (not the UI
    // accent tokens): nudge green up, red/blue slightly down.
    var tint = function (c) {
      return [
        Math.max(0, Math.min(255, Math.round(c[0] * 0.97))),
        Math.max(0, Math.min(255, Math.round(c[1] * 1.05))),
        Math.max(0, Math.min(255, Math.round(c[2] * 0.98))),
      ];
    };
    var ramp = function (names) { return names.map(function (p) { return tint(hexToRgb(pick(p[0], p[1]))); }); };
    return {
      warm: ramp([
        ["--clay", "#9c5a44"],
        ["--terracotta", "#b5623b"],
        ["--ochre", "#c98a3a"],
        ["--sand", "#d8c39c"],
        ["--paper-2", "#e3d8c2"],
      ]),
      green: ramp([
        ["--moss-deep", "#46512f"],
        ["--moss", "#6b7145"],
        ["--olive", "#8b8b4e"],
        ["--tan", "#c3b184"],
        ["--paper-2", "#e3d8c2"],
      ]),
    };
  }

  function hexToRgb(hex) {
    hex = hex.replace("#", "");
    if (hex.length === 3) {
      hex = hex[0] + hex[0] + hex[1] + hex[1] + hex[2] + hex[2];
    }
    var n = parseInt(hex, 16);
    return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
  }

  /* ── Bayer 8x8 threshold matrix, normalised to 0..1. */
  var BAYER = (function () {
    var base = [
      [0, 32, 8, 40, 2, 34, 10, 42],
      [48, 16, 56, 24, 50, 18, 58, 26],
      [12, 44, 4, 36, 14, 46, 6, 38],
      [60, 28, 52, 20, 62, 30, 54, 22],
      [3, 35, 11, 43, 1, 33, 9, 41],
      [51, 19, 59, 27, 49, 17, 57, 25],
      [15, 47, 7, 39, 13, 45, 5, 37],
      [63, 31, 55, 23, 61, 29, 53, 21],
    ];
    var m = [];
    for (var y = 0; y < 8; y++) {
      m[y] = [];
      for (var x = 0; x < 8; x++) m[y][x] = (base[y][x] + 0.5) / 64;
    }
    return m;
  })();

  /* ── Fractal value noise (deterministic, no Math.random). ──
        Integer-hashed lattice → smooth value noise → fBm → domain
        warp, for an organic marbled field instead of banded sines. */
  function hash2(ix, iy) {
    var n = Math.imul(ix | 0, 374761393) ^ Math.imul(iy | 0, 668265263);
    n = Math.imul(n ^ (n >>> 13), 1274126177);
    n ^= n >>> 16;
    return (n >>> 0) / 4294967296;
  }
  function vnoise(x, y) {
    var x0 = Math.floor(x), y0 = Math.floor(y);
    var fx = x - x0, fy = y - y0;
    var ux = fx * fx * (3 - 2 * fx);
    var uy = fy * fy * (3 - 2 * fy);
    var a = hash2(x0, y0), b = hash2(x0 + 1, y0);
    var c = hash2(x0, y0 + 1), d = hash2(x0 + 1, y0 + 1);
    return a + (b - a) * ux + (c - a) * uy + (a - b - c + d) * ux * uy;
  }
  function fbm(x, y) {
    var v = 0, amp = 0.5, freq = 1;
    for (var o = 0; o < 3; o++) {
      v += amp * vnoise(x * freq, y * freq);
      freq *= 2.02;
      amp *= 0.5;
    }
    return v; // ~0..0.875
  }
  // cheaper 2-octave fBm for the large-scale region field
  function fbm2(x, y) {
    return 0.62 * vnoise(x, y) + 0.30 * vnoise(x * 2.02, y * 2.02);
  }

  var canvas = document.querySelector(".hero-canvas");
  if (canvas && canvas.getContext) {
    setupDither(canvas);
  }

  function setupDither(display) {
    var dctx = display.getContext("2d");
    // low-resolution offscreen buffer → scaled up into chunky pixels
    var buf = document.createElement("canvas");
    var bctx = buf.getContext("2d");
    var PIXEL = 4; // approximate on-screen size of one dither cell (smaller = denser)
    var palette = readPalette();
    var img, W, H;

    function resize() {
      var r = display.getBoundingClientRect();
      var cssW = Math.max(1, Math.round(r.width));
      var cssH = Math.max(1, Math.round(r.height));
      W = Math.max(16, Math.ceil(cssW / PIXEL));
      H = Math.max(16, Math.ceil(cssH / PIXEL));
      buf.width = W;
      buf.height = H;
      display.width = W;
      display.height = H;
      dctx.imageSmoothingEnabled = false;
      palette = readPalette();
      img = bctx.createImageData(W, H);
      waveA = new Float32Array(W * H);
      waveB = new Float32Array(W * H);
      lastCellX = -1;
    }

    /* ── Pointer ripples: a damped wave automaton on the dither grid.
          The pointer dents the height field; the dent propagates as
          rings that get dithered into the same ramps as the terrain. */
    var waveA, waveB; // waveA = current heights, waveB = previous
    var WAVE_DAMP = 0.975;
    var WAVE_WARP = 14;   // wave gradient displaces the noise-sampling coords (refraction)
    var WAVE_SHADE = 0.1; // faint crest/trough shading on top of the refraction
    var lastCellX = -1, lastCellY = -1;

    // Soft 5x5 dent: a wider impulse makes longer-wavelength rings
    // that survive the busy terrain instead of vanishing into it.
    function splash(cx, cy, strength) {
      var x = cx | 0, y = cy | 0;
      if (x < 2 || y < 2 || x >= W - 2 || y >= H - 2) return;
      for (var dy = -2; dy <= 2; dy++) {
        var row = (y + dy) * W + x;
        for (var dx = -2; dx <= 2; dx++) {
          var w = 1 - (dx * dx + dy * dy) / 6;
          if (w > 0) waveA[row + dx] -= strength * w;
        }
      }
    }

    function stepWaves() {
      var a = waveA, b = waveB;
      for (var y = 1; y < H - 1; y++) {
        var row = y * W;
        for (var x = 1; x < W - 1; x++) {
          var i = row + x;
          b[i] = ((a[i - 1] + a[i + 1] + a[i - W] + a[i + W]) * 0.5 - b[i]) * WAVE_DAMP;
        }
      }
      waveA = b;
      waveB = a;
    }

    function pointerCell(e) {
      var r = display.getBoundingClientRect();
      return [
        ((e.clientX - r.left) / r.width) * W,
        ((e.clientY - r.top) / r.height) * H,
      ];
    }

    // Listen on the hero (not the canvas) so the trail continues
    // under the floating panel instead of dying at its edge.
    var hero = display.parentElement;
    hero.addEventListener("pointermove", function (e) {
      if (reduced.matches || !raf) return;
      var c = pointerCell(e);
      if (lastCellX >= 0) {
        var dx = c[0] - lastCellX;
        var dy = c[1] - lastCellY;
        var steps = Math.min(48, Math.ceil(Math.max(Math.abs(dx), Math.abs(dy))) || 1);
        for (var s = 1; s <= steps; s++) {
          splash(lastCellX + (dx * s) / steps, lastCellY + (dy * s) / steps, 0.4);
        }
      } else {
        splash(c[0], c[1], 0.4);
      }
      lastCellX = c[0];
      lastCellY = c[1];
    });
    hero.addEventListener("pointerdown", function (e) {
      if (reduced.matches || !raf) return;
      var c = pointerCell(e);
      splash(c[0], c[1], 2);
    });
    hero.addEventListener("pointerleave", function () { lastCellX = -1; });
    hero.addEventListener("pointercancel", function () { lastCellX = -1; });

    // Per-load random seed: shifts sampling into a different region of
    // the infinite noise field so the background doesn't start the same
    // place every visit. (Browser Math.random — fine here.)
    var SEED_X = Math.random() * 940 + 30;
    var SEED_Y = Math.random() * 940 + 30;

    // Domain-warped fBm: two warp layers churned by time give an
    // organic, marbled, non-repeating field.
    var FREQ = 0.014; // feature scale in cells (smaller = larger blobs)
    function field(x, y, t) {
      var nx = x * FREQ + SEED_X;
      var ny = y * FREQ + SEED_Y;
      // first warp vector
      var qx = fbm(nx, ny + t * 0.05);
      var qy = fbm(nx + 5.2, ny + 1.3 - t * 0.04);
      // second warp vector, offset by the first
      var rx = fbm(nx + 3.4 * qx + 1.7, ny + 3.4 * qy + 9.2 + t * 0.03);
      var ry = fbm(nx + 3.4 * qx + 8.3, ny + 3.4 * qy + 2.8 - t * 0.035);
      // final sample, plus a gentle diagonal bias for large-scale depth
      var v = fbm(nx + 3.2 * rx, ny + 3.2 * ry);
      v = v * 1.18 + 0.14 * (rx - ry);
      v = (v - 0.14) / 0.66; // normalise roughly to 0..1
      if (v < 0) v = 0;
      else if (v > 1) v = 1;
      return v;
    }

    // Large-scale region field: chooses warm-earth vs green-tan patches.
    var RFREQ = 0.006;
    function regionField(x, y, t) {
      var v = fbm2(x * RFREQ + SEED_X * 0.6 + 11.5 + t * 0.02, y * RFREQ + SEED_Y * 0.6 + 4.2 - t * 0.016);
      v = (v - 0.16) / 0.6;
      if (v < 0) v = 0;
      else if (v > 1) v = 1;
      return v;
    }

    function render(t) {
      var data = img.data;
      var warm = palette.warm;
      var green = palette.green;
      var last = warm.length - 1;
      var i = 0;
      for (var y = 0; y < H; y++) {
        var brow = BAYER[y & 7];
        var brow2 = BAYER[(y + 4) & 7];
        for (var x = 0; x < W; x++) {
          // refract the terrain through the wave: the height-field
          // gradient bends where this cell samples the noise, so the
          // marbling itself churns inside the rings.
          var wi = i >> 2;
          var wx = x, wy = y;
          if (x > 0 && x < W - 1 && y > 0 && y < H - 1) {
            wx += (waveA[wi + 1] - waveA[wi - 1]) * WAVE_WARP;
            wy += (waveA[wi + W] - waveA[wi - W]) * WAVE_WARP;
          }
          var v = field(wx, wy, t) + waveA[wi] * WAVE_SHADE;
          if (v < 0) v = 0;
          else if (v > 1) v = 1;
          // ordered-dither the luminance ramp between neighbouring stops
          var scaled = v * last;
          var lo = Math.floor(scaled);
          if (lo >= last) lo = last - 1;
          if (lo < 0) lo = 0;
          var frac = scaled - lo;
          var idx = frac > brow[x & 7] ? lo + 1 : lo;
          // ordered-dither the region to pick which ramp this pixel uses
          var region = regionField(wx, wy, t);
          var arr = region > brow2[(x + 4) & 7] ? warm : green;
          var c = arr[idx];
          data[i] = c[0];
          data[i + 1] = c[1];
          data[i + 2] = c[2];
          data[i + 3] = 255;
          i += 4;
        }
      }
      bctx.putImageData(img, 0, 0);
      dctx.drawImage(buf, 0, 0);
    }

    var reduced = window.matchMedia("(prefers-reduced-motion: reduce)");
    var raf = 0;
    var start = null;
    var lastDraw = -1e9;
    var FRAME_MS = 33; // ~30fps — the field drifts slowly, so this is plenty

    function loop(now) {
      if (start === null) start = now;
      if (now - lastDraw >= FRAME_MS) {
        stepWaves();
        stepWaves(); // two sim ticks per drawn frame: rings spread faster
        render((now - start) / 1000);
        lastDraw = now;
      }
      raf = requestAnimationFrame(loop);
    }

    function begin() {
      cancelAnimationFrame(raf);
      resize();
      if (reduced.matches) {
        render(6); // one representative static frame
      } else {
        start = null;
        raf = requestAnimationFrame(loop);
      }
    }

    var rt;
    window.addEventListener("resize", function () {
      clearTimeout(rt);
      rt = setTimeout(begin, 150);
    });
    if (reduced.addEventListener) reduced.addEventListener("change", begin);
    // pause when the tab/section is off-screen to save cycles
    if ("IntersectionObserver" in window) {
      new IntersectionObserver(function (entries) {
        entries.forEach(function (e) {
          if (reduced.matches) return;
          if (e.isIntersecting) {
            if (!raf) { start = null; raf = requestAnimationFrame(loop); }
          } else {
            cancelAnimationFrame(raf);
            raf = 0;
          }
        });
      }, { threshold: 0 }).observe(display);
    }

    begin();
  }

  /* ── Client / host install toggle: swap the command live. ──
        TODO(install-script): confirm exact one-liners once goq.sh
        serves the install scripts (client app + host runtime pkg). */
  var INSTALL_CMD = {
    client: "curl -fsSL https://goq.sh | sh",
    host: "curl -fsSL https://goq.sh/host | sh",
  };
  document.querySelectorAll('input[name="install-target"]').forEach(function (radio) {
    radio.addEventListener("change", function () {
      if (!radio.checked) return;
      var cmd = INSTALL_CMD[radio.value] || INSTALL_CMD.client;
      var box = document.querySelector(".install");
      if (!box) return;
      box.setAttribute("data-copy", cmd);
      var textEl = box.querySelector(".install-text");
      if (textEl) textEl.textContent = cmd;
    });
  });

  /* ── Copy-to-clipboard for the install command(s). ── */
  document.querySelectorAll(".install").forEach(function (box) {
    var btn = box.querySelector(".install-copy");
    if (!btn) return;
    btn.addEventListener("click", function () {
      var text = box.getAttribute("data-copy") || ""; // read current selection
      var done = function () {
        btn.textContent = "copied";
        btn.classList.add("copied");
        setTimeout(function () {
          btn.textContent = "copy";
          btn.classList.remove("copied");
        }, 1600);
      };
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(done, fallback);
      } else {
        fallback();
      }
      function fallback() {
        var ta = document.createElement("textarea");
        ta.value = text;
        ta.style.position = "fixed";
        ta.style.opacity = "0";
        document.body.appendChild(ta);
        ta.select();
        try { document.execCommand("copy"); done(); } catch (e) {}
        document.body.removeChild(ta);
      }
    });
  });

  /* ── Scroll reveal: gated behind html.js so no-JS visitors see
        everything; staggered per sibling group via a CSS var. ── */
  document.documentElement.classList.add("js");
  var revealEls = document.querySelectorAll(".reveal");
  if ("IntersectionObserver" in window && revealEls.length) {
    // stagger within each parent group (feature grid, steps, diagram)
    var groups = new Map();
    revealEls.forEach(function (el) {
      var parent = el.parentElement;
      var n = groups.get(parent) || 0;
      el.style.setProperty("--reveal-delay", (n * 0.07).toFixed(2) + "s");
      groups.set(parent, n + 1);
    });
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          e.target.classList.add("in");
          io.unobserve(e.target);
        }
      });
    }, { threshold: 0.15, rootMargin: "0px 0px -5% 0px" });
    revealEls.forEach(function (el) { io.observe(el); });
  } else {
    revealEls.forEach(function (el) { el.classList.add("in"); });
  }
})();
