      // 部分翻译:en 完整,zh-CN 只提供 greeting(且宿主 JSON 的 zh-CN 也只译了
      // app.title)。其余 key 在 zh-CN 缺失 → 页面按 pick() 回落 en,宿主标题按
      // resolve_title 回落 default(en)/fallback。这表示"缺非默认译文/key 是
      // 可接受的部分翻译"(规格 §3.2),自然重新打开取得新 locale,不因切换自动 reload。
      var STRINGS = {
        en: {
          greeting: "Hello, world!",
          hint: "This app gets the language once at startup (initial), not on later switches.",
        },
        "zh-CN": {
          greeting: "你好，世界！", // 故意不提供 hint → 下方 pick() 回落 en
        },
      };

      function pick(loc, key) {
        var table = STRINGS[loc];
        if (table && table[key] != null) return table[key];
        return STRINGS.en[key];
      }

      function render(snap) {
        if (!snap) return;
        var loc = snap.app_locale;
        document.documentElement.lang = loc;
        document.getElementById("greeting").textContent = pick(loc, "greeting");
        document.getElementById("hint").textContent = pick(loc, "hint");
        document.getElementById("meta").textContent =
          "ui_locale=" + snap.ui_locale + " app_locale=" + snap.app_locale +
          " revision=" + snap.locale_revision + " (initial: no live updates)";
      }

      // initial:桥只在文档初始化时提供一次语言;subscribe 立即回调一次,不接收同文档内变化。
      var bridge = window.bytehost && window.bytehost.i18n;
      if (bridge) {
        render(bridge.getSnapshot());
        bridge.subscribe(render);
      } else {
        render({ version: 1, ui_locale: "en", app_locale: "en", locale_revision: "0" });
      }
    
