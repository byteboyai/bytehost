      // 页面自身的字符串表:语言桥只告诉页面"用哪个语言",字符串由页面自备
      // (规格 §3.1:页面可以复用宿主 JSON,也可以用自己内部的 i18n 系统)。
      // 关键区分:用 app_locale(宿主协商结果)渲染,而非 ui_locale(宿主期望)。
      var STRINGS = {
        en: {
          greeting: "Hello, world!",
          hint: "This app receives language changes live via the bytehost i18n bridge. Type in the box above and scroll the log — neither is reset on switch.",
          logLine: "Scrollable line — your scroll position is preserved across a language hot-swap.",
        },
        "zh-CN": {
          greeting: "你好，世界！",
          hint: "本应用通过 bytehost 语言桥实时接收语言变化。在上方输入框输入、滚动下方日志——切换语言时两者都不会被重置。",
          logLine: "可滚动行——语言热切换时你的滚动位置会被保留。",
        },
      };

      // 只生成一次长内容,供滚动演示;与语言无关,不随 render 重建。
      var log = document.getElementById("log");
      for (var i = 0; i < 60; i++) {
        var p = document.createElement("div");
        p.textContent = (i + 1) + ". " + STRINGS.en.logLine;
        log.appendChild(p);
      }

      function render(snap) {
        if (!snap) return; // 首帧到达前 getSnapshot() 返回 null(pending),不渲染过期值
        var loc = snap.app_locale;
        document.documentElement.lang = loc;
        var s = STRINGS[loc] || STRINGS.en;
        document.getElementById("greeting").textContent = s.greeting;
        document.getElementById("hint").textContent = s.hint;
        document.getElementById("meta").textContent =
          "ui_locale=" + snap.ui_locale + " app_locale=" + snap.app_locale +
          " revision=" + snap.locale_revision + " version=" + snap.version;
      }

      // 只读桥:getSnapshot() 拿当前快照,subscribe() 注册回调(注册即立即回调当前快照)。
      var bridge = window.bytehost && window.bytehost.i18n;
      if (bridge) {
        render(bridge.getSnapshot());
        bridge.subscribe(render);
      } else {
        // 没有桥(宿主未声明 webview_locale,或旧宿主):用默认值兜底渲染。
        render({ version: 1, ui_locale: "en", app_locale: "en", locale_revision: "0" });
      }
    
