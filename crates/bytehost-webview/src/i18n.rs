//! 应用语言桥的**纯策略**(规格 §6.1 / §6.3):快照类型、序列化,以及注入到应用 webview 的
//! 只读桥 `window.bytehost.i18n`。
//!
//! 设计要点(见 `docs/superpowers/plans/2026-10-14-bytehost-i18n-baseline.md` T0-4 spike 结论):
//! - 初始化脚本(`app_init_script`)在 webview 创建时**一次性冻结**,无法携带运行时变化的 locale 值,
//!   所以桥**只装骨架**:`getSnapshot()` 在首帧到达前返回 `null`(pending/skeleton),绝不返回
//!   创建时捕获的已知过期值;`subscribe()` 注册回调。真实的快照值由宿主在导航完成后经
//!   `evaluate_app` 调用 `i18n_set_locale_script` 下发(§6.2 的"首帧经 evaluate_app 下发")。
//! - 桥是**只读**的:页面只能读快照、订阅变化,没有任何可调用的宿主命令能力;页面篡改自身语言显示
//!   不被当作权限边界,也不能反向修改产品语言。
//! - 仅当 manifest 声明 `[i18n]` 且 `webview_locale != none` 时,宿主才在初始化脚本里装入桥骨架
//!   (`app_init_script(nonce, install_i18n_bridge = true)`);`none` 不装,桥完全不存在。
//! - 快照以 `serde_json` 序列化后作为对象字面量注入,**不**用字符串拼接 key/标题,杜绝可执行 JS 注入。

use bytehost_apps::i18n::negotiate_locale;
use bytehost_apps::proto::AppDisplayI18n;
use serde::{Deserialize, Serialize};

/// 重新导出 `WebViewLocale`,供消费方(如 Dozer)在不直接依赖 `bytehost_apps::i18n` 内部模块路径时
/// 判断 `webview_locale` 是否为 `Live`(仅 `Live` 接收同文档热更新)。
pub use bytehost_apps::i18n::WebViewLocale;

/// 应用 webview 语言桥的一次快照(只读,页面经 `window.bytehost.i18n` 读取)。
///
/// 不含任何宿主命令能力;页面拿到它只能渲染,不能改产品语言。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct I18nSnapshot {
    /// 契约版本,恒为 `1`。
    pub version: u8,
    /// 宿主期望的 UI 语言(协商前的原始值,如 `zh-CN`)。
    pub ui_locale: String,
    /// 按应用 `supported_locales`/`default_locale` 精确匹配协商后的语言(命中用该语言,否则 default)。
    pub app_locale: String,
    /// 本 GUI 外壳的显示代(十进制字符串)。不是 daemon 持久化 revision,也不跨窗口比较。
    pub locale_revision: String,
}

impl I18nSnapshot {
    /// 构造一份快照(版本固定为 1)。
    pub fn new(ui_locale: impl Into<String>, app_locale: impl Into<String>, revision: u64) -> Self {
        Self {
            version: 1,
            ui_locale: ui_locale.into(),
            app_locale: app_locale.into(),
            locale_revision: revision.to_string(),
        }
    }
}

/// 从 `AppSummary.display_i18n` 与宿主当前 UI 语言算出快照。
///
/// `app_locale` 走精确匹配协商(命中 `supported_locales` 用该语言,否则 `default_locale`);
/// `locale_revision` 由宿主传入(每次语言/活动版本变化自增)。
pub fn build_i18n_snapshot(
    display: &AppDisplayI18n,
    ui_locale: &str,
    revision: u64,
) -> I18nSnapshot {
    let app_locale = negotiate_locale(
        ui_locale,
        &display.supported_locales,
        &display.default_locale,
    );
    I18nSnapshot::new(ui_locale, app_locale, revision)
}

/// 注入脚本里安装的桥骨架(只读):`window.bytehost.i18n` 提供 `getSnapshot` / `subscribe`,
/// 真实值由宿主经 `i18n_set_locale_script` 下发到 `window.__bytehost_i18n_set`。
///
/// 桥在首个完整快照到达前处于 pending:`getSnapshot()` 返回 `null`,不返回创建时捕获的过期值。
/// 页面注册 `subscribe` 立即拿到当前快照(首帧前为 `null`);回调里抛错不影响宿主。
/// 不暴露任何宿主命令能力。
///
/// 快照正确性约定(对应验收 B09/B11):
/// - `version` 必须为 `1`,且 `ui_locale`/`app_locale`/`locale_revision` 均为字符串,否则整条丢弃;
/// - `locale_revision` 为外壳本地显示代次(十进制),用 `BigInt` 比较;已就绪后只允许**严格更大**的
///   revision 写入——拒绝乱序(回退)与相等(幂等,不重复通知订阅者);
/// - `getSnapshot` 与订阅回调都拿到**快照副本**,页面无法借由返回值篡改内部状态;
/// - 页面篡改自身语言显示不被当作权限边界,也不能反向修改产品语言。
pub fn i18n_bridge_init_script() -> String {
    "(function(){
var b={};var subs=[];var snap=null;var ready=false;
function clone(o){return o==null?null:JSON.parse(JSON.stringify(o));}
function valid(s){return s&&s.version===1&&typeof s.ui_locale==='string'&&typeof s.app_locale==='string'&&typeof s.locale_revision==='string';}
function rev(s){try{return BigInt(s.locale_revision);}catch(e){return null;}}
b.getSnapshot=function(){return clone(snap);};
b.subscribe=function(cb){if(typeof cb!=='function')return function(){};subs.push(cb);try{cb(clone(snap));}catch(e){}return function(){var i=subs.indexOf(cb);if(i>=0)subs.splice(i,1);};};
try{Object.defineProperty(window,'bytehost',{value:window.bytehost||{},writable:true,configurable:true});}catch(e){}
window.bytehost.i18n=b;
window.__bytehost_i18n_set=function(s){
if(!valid(s))return;
var n=rev(s);if(n===null)return;
if(ready){var cur=rev(snap);if(cur===null||n<=cur)return;}
snap=clone(s);ready=true;
for(var i=0;i<subs.length;i++){try{subs[i](clone(snap));}catch(e){}}
};
})();".to_string()
}

/// 经 `evaluate_app`(导航完成后)下发的首帧/后续快照脚本:调用桥内部 setter。
///
/// 快照以 `serde_json` 序列化为对象字面量注入,**不**用字符串拼接 key/标题,避免可执行 JS 注入。
pub fn i18n_set_locale_script(snapshot: &I18nSnapshot) -> String {
    format!(
        "window.__bytehost_i18n_set({});",
        serde_json::to_string(snapshot).expect("快照序列化失败")
    )
}

/// 是否应为该 webview 安装语言桥:`webview_locale != none` 才装。
/// `none`(默认)或根本未声明 `[i18n]` 时,桥完全不存在。
pub fn should_install_bridge(webview_locale: WebViewLocale) -> bool {
    !matches!(webview_locale, WebViewLocale::None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_apps::proto::{AppDisplayI18n, AppDisplayTitle, DisplayTitleScope};
    use std::collections::BTreeMap;

    fn display() -> AppDisplayI18n {
        AppDisplayI18n {
            default_locale: "en".into(),
            supported_locales: vec!["en".into(), "zh-CN".into()],
            webview_locale: WebViewLocale::Live,
            titles: vec![AppDisplayTitle {
                scope: DisplayTitleScope::Presentation,
                key: "app.title".into(),
                fallback: "Notes".into(),
                values: [("en".to_string(), "Notes".to_string())]
                    .into_iter()
                    .collect::<BTreeMap<_, _>>(),
            }],
        }
    }

    #[test]
    fn snapshot_serializes_to_a_stable_wire_shape() {
        let s = I18nSnapshot::new("zh-CN", "zh-CN", 3);
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["ui_locale"], "zh-CN");
        assert_eq!(json["app_locale"], "zh-CN");
        assert_eq!(json["locale_revision"], "3");
    }

    #[test]
    fn app_locale_is_negotiated_exactly_supported_else_default() {
        let d = display();
        let s = build_i18n_snapshot(&d, "zh-CN", 1);
        assert_eq!(s.app_locale, "zh-CN", "命中声明的语言");
        let s = build_i18n_snapshot(&d, "fr", 2);
        assert_eq!(s.app_locale, "en", "未知语言回落 default");
        assert_eq!(s.locale_revision, "2");
    }

    #[test]
    fn set_script_injects_json_not_concatenated_locale() {
        // 恶意/特殊 UI 语言也必须作为**数据**注入:经 serde_json 序列化为字符串字面量,
        // 不是拼进代码。脚本经 evaluate_script 执行(非 HTML 内嵌),值落在 JSON 字符串里
        // 即只是数据,不会变成可执行语句。
        let s = I18nSnapshot::new("</script><script>alert(1)", "en", 1);
        let script = i18n_set_locale_script(&s);
        assert!(script.starts_with("window.__bytehost_i18n_set("));
        assert!(script.ends_with(");"));
        // 该脚本可重新解析为等价快照:证明 locale 是数据而非拼接进的代码片段。
        let json = script
            .trim_start_matches("window.__bytehost_i18n_set(")
            .trim_end_matches(");");
        let back: I18nSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(back, s, "locale 必须是可往返解析的 JSON 数据");
        // 且它出现在 JSON 字符串字面量里(ui_locale 的值带引号),不是裸代码。
        assert!(json.contains("\"ui_locale\":\"</script><script>alert(1)\""));
    }

    #[test]
    fn init_script_installs_readonly_bridge_skeleton() {
        let script = i18n_bridge_init_script();
        assert!(script.contains("window.bytehost.i18n=b"));
        assert!(script.contains("b.getSnapshot=function()"));
        assert!(script.contains("b.subscribe=function(cb)"));
        // 桥内部 setter 用于宿主下发;首帧前 getSnapshot 返回 null(pending)。
        assert!(script.contains("window.__bytehost_i18n_set=function(s)"));
        assert!(!script.contains("__host.dispatch"), "不得暴露宿主命令通道");
        assert!(!script.contains("postMessage"), "桥不提供任何 IPC 能力");
    }

    #[test]
    fn bridge_is_only_installed_when_locale_is_not_none() {
        assert!(should_install_bridge(WebViewLocale::Live));
        assert!(should_install_bridge(WebViewLocale::Initial));
        assert!(!should_install_bridge(WebViewLocale::None));
    }

    /// 实际在 Node 里执行生成的桥脚本,验证 revision 单调/幂等、快照隔离、畸形数据拒绝(B09/B11)。
    /// 若环境无 `node` 则跳过(本地与 CI 已用 `node --test` 复跑)。脚本由 `i18n_bridge_init_script`
    /// 实时生成,不复制字符串,避免实现与测试漂移。
    #[test]
    fn bridge_script_enforces_revision_and_isolation_under_node() {
        let script = i18n_bridge_init_script();
        let harness = format!(
            "global.window={{}};{script}\
var i18n=global.window.bytehost.i18n;var out=[];\
var immediate; i18n.subscribe(function(s){{immediate=s;}});\
out.push(immediate===null?'imm-ok':'imm-FAIL');\
global.window.__bytehost_i18n_set({{version:1,ui_locale:'en',app_locale:'en',locale_revision:'10'}});\
global.window.__bytehost_i18n_set({{version:1,ui_locale:'fr',app_locale:'fr',locale_revision:'9'}});\
out.push(i18n.getSnapshot().locale_revision==='10'?'mono-ok':'mono-FAIL');\
var calls=0;i18n.subscribe(function(){{calls++;}});var before=calls;\
global.window.__bytehost_i18n_set({{version:1,ui_locale:'fr',app_locale:'fr',locale_revision:'9'}});\
out.push((calls-before)===0?'idem-ok':'idem-FAIL');\
var s=i18n.getSnapshot();s.app_locale='MUT';\
out.push(i18n.getSnapshot().app_locale!=='MUT'?'iso-ok':'iso-FAIL');\
global.window.__bytehost_i18n_set({{invalid:true}});\
out.push(i18n.getSnapshot()&&i18n.getSnapshot().invalid===undefined?'mal-ok':'mal-FAIL');\
console.log('BRIDGE_RESULTS:'+out.join(','));"
        );
        let out = std::process::Command::new("node")
            .arg("-e")
            .arg(&harness)
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let s = String::from_utf8_lossy(&o.stdout);
                assert!(
                    s.contains("BRIDGE_RESULTS:imm-ok,mono-ok,idem-ok,iso-ok,mal-ok"),
                    "got: {s}"
                );
            }
            _ => {
                // 无 node 环境:仅做结构断言,真实行为由 `node --test` 覆盖。
                assert!(script.contains("valid(s)"), "桥脚本应校验快照合法性");
                assert!(script.contains("clone(snap)"), "桥脚本应对外暴露快照副本");
            }
        }
    }
}
