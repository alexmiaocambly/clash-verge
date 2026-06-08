//! macOS 原生托盘菜单（方向 A，P1）
//!
//! 用 objc2-app-kit 自建 `NSMenu` 并通过 `with_inner_tray_icon` → `ns_status_item().setMenu`
//! 接管现有 Tauri 托盘的菜单层。图标 / 左键行为 / 速率显示仍由现有逻辑管理。
//!
//! 菜单项 id 复用 `MenuIds`（与 muda 完全一致），点击事件统一经 `onMenuClick:`
//! 取出 `representedObject` 里的 id 字符串，再交给共享的 `super::dispatch_menu_id` 分发。
//!
//! 当前为 P1 第一版：仅实现代表性菜单项（普通项 / 勾选项 / 禁用态 / 子菜单），
//! 用于验证「原生菜单接管 + 事件分发」机制。订阅/代理等动态项后续补全。

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::Config;
use crate::core::handle::Handle;
use crate::process::AsyncHandler;
use crate::{Type, logging};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread as _, DefinedClass as _, MainThreadMarker, MainThreadOnly as _, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing as _, NSBaselineOffsetAttributeName, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSMenu, NSMenuItem,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSTextAlignment, NSTextTab, NSView,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSDictionary, NSMutableAttributedString, NSNumber, NSPoint, NSRange, NSRect, NSSize,
    NSString,
};
use tauri::tray::TrayIcon;
use tauri_plugin_mihomo::models::Proxies;

use super::menu_def::{MenuIds, MenuTexts};

/// 订阅项（owned，避免借用跨线程）
pub struct ProfileEntry {
    pub uid: String,
    pub name: String,
    pub is_current: bool,
}

/// 构建原生菜单所需的状态快照（均为 Send，可跨线程传入主线程回调）
pub struct NativeMenuData {
    pub mode: String,
    pub system_proxy: bool,
    pub tun_mode: bool,
    pub tun_available: bool,
    pub tray_speed: bool,
    pub lightweight: bool,
    pub profiles: Vec<ProfileEntry>,
    pub proxies: Option<Arc<Proxies>>,
    pub proxy_order: Option<HashMap<String, usize>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ClashVergeNativeMenuHandler"]
    struct MenuHandler;

    impl MenuHandler {
        /// 所有菜单项共用的 action：从 representedObject 取菜单 id，交共享分发逻辑处理
        #[unsafe(method(onMenuClick:))]
        fn on_menu_click(&self, sender: &NSMenuItem) {
            let id = sender
                .representedObject()
                .and_then(|obj| obj.downcast::<NSString>().ok())
                .map(|s| s.to_string());
            let Some(id) = id else {
                logging!(warn, Type::Tray, "[native] 菜单项缺少 id");
                return;
            };
            AsyncHandler::spawn(move || async move {
                super::dispatch_menu_id(&id).await;
            });
        }
    }
);

impl MenuHandler {
    fn new() -> Retained<Self> {
        unsafe { msg_send![Self::alloc(), init] }
    }
}

thread_local! {
    /// 菜单项的 target 是弱引用，handler 必须被持有，否则点击无响应
    static HANDLER: RefCell<Option<Retained<MenuHandler>>> = const { RefCell::new(None) };
}

fn handler(mtm: MainThreadMarker) -> Retained<MenuHandler> {
    let _ = mtm; // 仅用于标记主线程
    HANDLER.with(|h| {
        h.borrow_mut()
            .get_or_insert_with(MenuHandler::new)
            .clone()
    })
}

/// 一个代理组在当前菜单里的节点项：(延迟右对齐位置, [(节点名, 菜单项)])
type GroupNodeItems = (f64, Vec<(String, Retained<NSMenuItem>)>);

thread_local! {
    /// 每个代理组当前菜单里的节点项，供延迟测速后就地刷新
    static NODE_ITEMS: RefCell<HashMap<String, GroupNodeItems>> = RefCell::new(HashMap::new());
    /// 延迟测速按钮的 handler 保活（target 是弱引用）
    static DELAY_HANDLERS: RefCell<Vec<Retained<GroupDelayHandler>>> = const { RefCell::new(Vec::new()) };
}

struct GroupDelayIvars {
    group_name: String,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "ClashVergeGroupDelayHandler"]
    #[ivars = GroupDelayIvars]
    struct GroupDelayHandler;

    impl GroupDelayHandler {
        /// 「延迟测试」按钮 action：后台测速该组，完成后回主线程就地刷新各节点延迟（菜单不关闭）
        #[unsafe(method(onDelayTest:))]
        fn on_delay_test(&self, _sender: &NSButton) {
            let group = self.ivars().group_name.clone();
            logging!(info, Type::Tray, "[native] 延迟测试: {}", group);
            // 取该组当前节点列表，逐个并发测速，每个返回即刷新自己（逐个跳动）
            let nodes: Vec<String> = NODE_ITEMS.with(|m| {
                m.borrow()
                    .get(&group)
                    .map(|(_, v)| v.iter().map(|(n, _)| n.clone()).collect())
                    .unwrap_or_default()
            });
            AsyncHandler::spawn(move || async move {
                let verge = Config::verge().await.latest_arc();
                let test_url = verge
                    .default_latency_test
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("http://cp.cloudflare.com/generate_204")
                    .to_string();
                let timeout = verge.default_latency_timeout.filter(|t| *t > 0).unwrap_or(10000) as u32;
                for node in nodes {
                    let url = test_url.clone();
                    let group = group.clone();
                    AsyncHandler::spawn(move || async move {
                        let delay = match Handle::mihomo().await.delay_proxy_by_name(&node, &url, timeout).await {
                            Ok(d) => Some(d.delay),
                            Err(_) => Some(0), // 失败按超时（红）
                        };
                        let app = Handle::app_handle();
                        let _ = app.run_on_main_thread(move || update_node_delay(&group, &node, delay));
                    });
                }
            });
        }
    }
);

impl GroupDelayHandler {
    fn new(group_name: String) -> Retained<Self> {
        let this = Self::alloc().set_ivars(GroupDelayIvars { group_name });
        unsafe { msg_send![super(this), init] }
    }
}

/// 延迟值配文本与颜色：≤300 绿、>300 橙、0/超时 红、无数据灰
fn delay_color_and_text(delay: Option<u32>) -> (String, Retained<NSColor>) {
    let color = |r: f64, g: f64, b: f64| NSColor::colorWithRed_green_blue_alpha(r, g, b, 1.0);
    match delay {
        None => ("-ms".to_string(), color(0.48, 0.48, 0.51)),                 // 灰
        Some(0) => ("超时".to_string(), color(0.80, 0.21, 0.20)),             // 红
        Some(d) if d >= 10000 => ("超时".to_string(), color(0.80, 0.21, 0.20)),
        Some(d) if d > 300 => (format!("{d}ms"), color(0.82, 0.50, 0.08)),    // 橙
        Some(d) => (format!("{d}ms"), color(0.20, 0.57, 0.26)),               // 绿
    }
}

/// 节点行富文本标题：节点名 + 右对齐彩色延迟文字（纯文字 + tab，可靠右对齐）。
/// `location` 为延迟文字右边缘的对齐位置（该组最宽节点行宽度）。
fn node_attributed_title(node: &str, delay: Option<u32>, location: f64) -> Retained<NSMutableAttributedString> {
    let (delay_text, color) = delay_color_and_text(delay);
    let full = format!("{node}\t{delay_text}");
    let title = NSMutableAttributedString::from_nsstring(&NSString::from_str(&full));

    let node_tab_len = node.encode_utf16().count() + 1; // +1 为 tab 字符
    let full_len = full.encode_utf16().count();

    // 右对齐 tab，让延迟文字贴右
    let para = NSMutableParagraphStyle::new();
    let options: Retained<NSDictionary<NSString>> = NSDictionary::new();
    let tab = unsafe {
        NSTextTab::initWithTextAlignment_location_options(NSTextTab::alloc(), NSTextAlignment::Right, location, &options)
    };
    let tabs = NSArray::from_retained_slice(&[tab]);
    para.setTabStops(Some(&tabs));

    unsafe {
        title.addAttribute_value_range(NSParagraphStyleAttributeName, &para, NSRange::new(0, full_len));
        if full_len > node_tab_len {
            let delay_range = NSRange::new(node_tab_len, full_len - node_tab_len);
            // 延迟用更小字号、彩色，并上移基线与节点名垂直居中
            let delay_font = NSFont::systemFontOfSize(11.0);
            title.addAttribute_value_range(NSFontAttributeName, &delay_font, delay_range);
            title.addAttribute_value_range(NSForegroundColorAttributeName, &color, delay_range);
            title.addAttribute_value_range(NSBaselineOffsetAttributeName, &NSNumber::new_f64(1.0), delay_range);
        }
    }
    title
}

/// 在主线程就地刷新单个节点项的延迟（逐个跳动），菜单保持打开
fn update_node_delay(group: &str, node: &str, delay: Option<u32>) {
    NODE_ITEMS.with(|m| {
        if let Some((location, items)) = m.borrow().get(group)
            && let Some((_, item)) = items.iter().find(|(n, _)| n == node)
        {
            item.setAttributedTitle(Some(&node_attributed_title(node, delay, *location)));
        }
    });
}

/// 子菜单顶部「延迟测试」自定义行：点击按钮触发测速，菜单不关闭
fn add_delay_test_row(menu: &NSMenu, mtm: MainThreadMarker, group_name: &str, label: &str, width: f64) {
    // view 宽度只取够放按钮的最小值，菜单实际宽度由 NSMenu 的 minimumWidth 统一控制，
    // 避免短节点名组被按钮 view 撑宽、导致延迟右侧留大空白
    let view_w = width.max(140.0);
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(view_w, 26.0)));
    let button = NSButton::new(mtm);
    button.setTitle(&NSString::from_str(label));
    button.setFrame(NSRect::new(NSPoint::new(14.0, 1.0), NSSize::new(120.0, 24.0)));

    let handler = GroupDelayHandler::new(group_name.to_string());
    unsafe {
        button.setTarget(Some(&handler));
        button.setAction(Some(sel!(onDelayTest:)));
        view.addSubview(&button);
    }

    let item = NSMenuItem::new(mtm);
    item.setView(Some(&view));
    menu.addItem(&item);
    DELAY_HANDLERS.with(|v| v.borrow_mut().push(handler));
}

/// 创建一个普通/勾选菜单项并加入菜单，返回该项引用（供后续就地更新文本）
#[allow(clippy::too_many_arguments)]
fn add_item(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    handler: &MenuHandler,
    id: &str,
    title: &str,
    checked: bool,
    enabled: bool,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    // SAFETY: 在主线程操作刚创建的菜单项；id 字符串与全局持有的 handler 均有效
    unsafe {
        item.setRepresentedObject(Some(&NSString::from_str(id)));
        item.setTarget(Some(handler));
        item.setAction(Some(sel!(onMenuClick:)));
    }
    item.setState(if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    item.setEnabled(enabled);
    menu.addItem(&item);
    item
}

fn add_separator(menu: &NSMenu, mtm: MainThreadMarker) {
    menu.addItem(&NSMenuItem::separatorItem(mtm));
}

/// 节点菜单项：彩色延迟 + 点击切换 + 选中勾选
#[allow(clippy::too_many_arguments)]
fn add_node_item(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    handler: &MenuHandler,
    group_name: &str,
    node: &str,
    delay: Option<u32>,
    checked: bool,
    location: f64,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setAttributedTitle(Some(&node_attributed_title(node, delay, location)));
    // SAFETY: 在主线程操作刚创建的菜单项；id 字符串与全局持有的 handler 均有效
    unsafe {
        item.setRepresentedObject(Some(&NSString::from_str(&format!("proxy_{group_name}_{node}"))));
        item.setTarget(Some(handler));
        item.setAction(Some(sel!(onMenuClick:)));
    }
    item.setState(if checked {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
    menu.addItem(&item);
    item
}

/// 把一个子菜单作为带标题的菜单项加入父菜单
fn add_submenu(menu: &NSMenu, mtm: MainThreadMarker, title: &str, submenu: &NSMenu) {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    item.setSubmenu(Some(submenu));
    menu.addItem(&item);
}

/// 构建原生 NSMenu
fn build_menu(mtm: MainThreadMarker, data: &NativeMenuData) -> Retained<NSMenu> {
    // 重建前清空上一份菜单的引用（节点项 / 测速 handler），避免持有失效项
    NODE_ITEMS.with(|m| m.borrow_mut().clear());
    DELAY_HANDLERS.with(|v| v.borrow_mut().clear());

    let texts = MenuTexts::new();
    let h = handler(mtm);
    let menu = NSMenu::new(mtm);

    // 仪表板
    add_item(&menu, mtm, &h, MenuIds::DASHBOARD, &texts.dashboard, false, true);
    add_separator(&menu, mtm);

    // 出站模式（勾选当前模式）
    add_item(&menu, mtm, &h, MenuIds::RULE_MODE, &texts.rule_mode, data.mode == "rule", true);
    add_item(&menu, mtm, &h, MenuIds::GLOBAL_MODE, &texts.global_mode, data.mode == "global", true);
    add_item(&menu, mtm, &h, MenuIds::DIRECT_MODE, &texts.direct_mode, data.mode == "direct", true);
    add_separator(&menu, mtm);

    // 代理组（ClashX 式：平铺到顶层，每行 = 组名 + 右侧当前节点 + 子菜单选节点）
    if let Some(proxies) = &data.proxies {
        add_proxy_groups_inline(&menu, mtm, &h, &texts, &data.mode, proxies, &data.proxy_order);
    }
    add_separator(&menu, mtm);

    // 系统代理 / TUN（勾选 + 可用性禁用态）
    add_item(&menu, mtm, &h, MenuIds::SYSTEM_PROXY, &texts.system_proxy, data.system_proxy, true);
    add_item(&menu, mtm, &h, MenuIds::TUN_MODE, &texts.tun_mode, data.tun_mode, data.tun_available);
    add_separator(&menu, mtm);

    // 订阅子菜单（位于 TUN 模式与轻量模式之间，用分隔线隔开）
    if !data.profiles.is_empty() {
        let profiles_submenu = NSMenu::new(mtm);
        for p in &data.profiles {
            add_item(
                &profiles_submenu,
                mtm,
                &h,
                &format!("profiles_{}", p.uid),
                &p.name,
                p.is_current,
                true,
            );
        }
        add_submenu(&menu, mtm, &texts.profiles, &profiles_submenu);
        add_separator(&menu, mtm);
    }

    // 显示实时速率（勾选）
    add_item(&menu, mtm, &h, MenuIds::TRAY_SPEED, &texts.tray_speed, data.tray_speed, true);
    // 轻量模式
    add_item(&menu, mtm, &h, MenuIds::LIGHTWEIGHT_MODE, &texts.lightweight_mode, data.lightweight, true);

    // 打开目录子菜单
    let dir_submenu = NSMenu::new(mtm);
    add_item(&dir_submenu, mtm, &h, MenuIds::CONF_DIR, &texts.conf_dir, false, true);
    add_item(&dir_submenu, mtm, &h, MenuIds::CORE_DIR, &texts.core_dir, false, true);
    add_item(&dir_submenu, mtm, &h, MenuIds::LOGS_DIR, &texts.logs_dir, false, true);
    add_item(&dir_submenu, mtm, &h, MenuIds::APP_LOG, &texts.app_log, false, true);
    add_item(&dir_submenu, mtm, &h, MenuIds::CORE_LOG, &texts.core_log, false, true);
    add_submenu(&menu, mtm, &texts.open_dir, &dir_submenu);

    // 更多子菜单
    let more_submenu = NSMenu::new(mtm);
    add_item(&more_submenu, mtm, &h, MenuIds::COPY_ENV, &texts.copy_env, false, true);
    add_item(&more_submenu, mtm, &h, MenuIds::CLOSE_ALL_CONNECTIONS, &texts.close_all_connections, false, true);
    add_item(&more_submenu, mtm, &h, MenuIds::RESTART_CLASH, &texts.restart_clash, false, true);
    add_item(&more_submenu, mtm, &h, MenuIds::RESTART_APP, &texts.restart_app, false, true);
    add_item(&more_submenu, mtm, &h, MenuIds::VERGE_VERSION, &texts.verge_version, false, true);
    add_submenu(&menu, mtm, &texts.more, &more_submenu);
    add_separator(&menu, mtm);

    // 退出
    add_item(&menu, mtm, &h, MenuIds::EXIT, &texts.exit, false, true);

    menu
}

/// 测量字符串在菜单字体下的渲染宽度（pt）
fn text_width(text: &str, font: &NSFont) -> f64 {
    let ns = NSString::from_str(text);
    let attrs = unsafe {
        let keys: &[&NSString] = &[NSFontAttributeName];
        let vals: &[&AnyObject] = &[font];
        NSDictionary::from_slices(keys, vals)
    };
    let attr =
        unsafe { NSAttributedString::initWithString_attributes(NSAttributedString::alloc(), &ns, Some(&attrs)) };
    attr.size().width
}

/// 构建「组名 + 右对齐灰色当前节点」的富文本标题（ClashX 式）。
/// `location` 为当前节点的右对齐位置，统一取最宽组的宽度，使节点紧贴子菜单箭头。
fn group_title(group_name: &str, now: &str, location: f64) -> Retained<NSMutableAttributedString> {
    let full = format!("{group_name}\t{now}");
    let ns = NSString::from_str(&full);
    let attr = NSMutableAttributedString::from_nsstring(&ns);

    let para = NSMutableParagraphStyle::new();
    let options: Retained<NSDictionary<NSString>> = NSDictionary::new();
    let tab = unsafe {
        NSTextTab::initWithTextAlignment_location_options(NSTextTab::alloc(), NSTextAlignment::Right, location, &options)
    };
    let tabs = NSArray::from_retained_slice(&[tab]);
    para.setTabStops(Some(&tabs));

    let full_len = full.encode_utf16().count();
    let group_len = group_name.encode_utf16().count();
    unsafe {
        attr.addAttribute_value_range(NSParagraphStyleAttributeName, &para, NSRange::new(0, full_len));
        if full_len > group_len {
            attr.addAttribute_value_range(
                NSForegroundColorAttributeName,
                &NSColor::secondaryLabelColor(),
                NSRange::new(group_len, full_len - group_len),
            );
        }
    }
    attr
}

/// 构建某个代理组的子菜单：顶部「延迟测试」+ 节点列表（带延迟、当前节点勾选）
fn build_group_submenu(
    mtm: MainThreadMarker,
    h: &MenuHandler,
    texts: &MenuTexts,
    group_name: &str,
    all: &[String],
    now: &str,
    proxies: &Proxies,
) -> Retained<NSMenu> {
    let group_menu = NSMenu::new(mtm);

    // 收集节点初始延迟，测量最宽节点行（节点名 + 间隔 + 延迟文字）作为延迟右对齐位置
    const NODE_GAP: f64 = 24.0;
    let menu_font = NSFont::menuFontOfSize(0.0);
    let entries: Vec<(String, Option<u32>, bool)> = all
        .iter()
        .map(|node| {
            let delay = proxies
                .proxies
                .get(node)
                .and_then(|n| n.history.last())
                .map(|hh| u32::from(hh.delay))
                .filter(|d| *d != 0);
            (node.clone(), delay, node.as_str() == now)
        })
        .collect();
    let location = entries
        .iter()
        .map(|(node, delay, _)| {
            let (delay_text, _) = delay_color_and_text(*delay);
            text_width(node, &menu_font) + NODE_GAP + text_width(&delay_text, &menu_font)
        })
        .fold(0.0_f64, f64::max);

    // 强制子菜单最小宽度，使右对齐的延迟文字不被裁剪（custom view 宽度无法撑宽 NSMenu，须用 minimumWidth）
    group_menu.setMinimumWidth(location + 38.0);

    // 顶部「延迟测试」自定义行：点击不关闭菜单、实时刷新本组延迟
    add_delay_test_row(&group_menu, mtm, group_name, &texts.delay_test, location);

    let mut node_items: Vec<(String, Retained<NSMenuItem>)> = Vec::with_capacity(entries.len());
    for (node, delay, checked) in &entries {
        let item = add_node_item(&group_menu, mtm, h, group_name, node, *delay, *checked, location);
        node_items.push((node.clone(), item));
    }
    // 记录本组徽章右对齐位置与节点项引用，供测速后就地刷新
    NODE_ITEMS.with(|m| m.borrow_mut().insert(group_name.to_string(), (location, node_items)));
    group_menu
}

/// 把代理组平铺加入顶层菜单：每组一行（组名 + 右侧当前节点 + 子菜单选节点），按 runtime 顺序排序
fn add_proxy_groups_inline(
    menu: &NSMenu,
    mtm: MainThreadMarker,
    h: &MenuHandler,
    texts: &MenuTexts,
    mode: &str,
    proxies: &Proxies,
    order: &Option<HashMap<String, usize>>,
) {
    // 组名与当前节点之间的最小留白
    const GROUP_NODE_GAP: f64 = 40.0;
    let font = NSFont::menuFontOfSize(0.0);

    // 第一遍：过滤、构建子菜单、测量最宽组（组名 + 间隔 + 当前节点）
    let mut collected: Vec<(String, String, Retained<NSMenu>)> = Vec::new();
    let mut max_width: f64 = 0.0;
    for (group_name, group_data) in proxies.proxies.iter() {
        let should_show = match mode {
            "global" => group_name == "GLOBAL",
            _ => group_name != "GLOBAL",
        } && !group_data.hidden.unwrap_or_default();
        if !should_show {
            continue;
        }
        let Some(all) = group_data.all.as_ref() else {
            continue;
        };
        if all.is_empty() {
            continue;
        }
        let now = group_data.now.as_deref().unwrap_or_default();

        let width = text_width(group_name, &font) + GROUP_NODE_GAP + text_width(now, &font);
        if width > max_width {
            max_width = width;
        }
        let submenu = build_group_submenu(mtm, h, texts, group_name, all, now, proxies);
        collected.push((group_name.to_string(), now.to_string(), submenu));
    }

    if let Some(order_map) = order {
        collected.sort_by(|(a, _, _), (b, _, _)| match (order_map.get(a), order_map.get(b)) {
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        });
    }

    // 第二遍：用统一的右对齐位置（最宽组宽度）构建菜单项，使当前节点紧贴箭头
    for (group_name, now, submenu) in &collected {
        let item = NSMenuItem::new(mtm);
        let title = group_title(group_name, now, max_width);
        item.setAttributedTitle(Some(&title));
        item.setSubmenu(Some(submenu));
        menu.addItem(&item);
    }
}

/// 用原生菜单刷新托盘。须能拿到 NSStatusItem（经 Tauri `with_inner_tray_icon`，回调在主线程）。
pub fn refresh(tray: &TrayIcon, data: NativeMenuData) {
    let result = tray.with_inner_tray_icon(move |inner| {
        let Some(mtm) = MainThreadMarker::new() else {
            logging!(warn, Type::Tray, "[native] 刷新跳过：非主线程");
            return;
        };
        let Some(status_item) = inner.ns_status_item() else {
            logging!(warn, Type::Tray, "[native] 刷新跳过：无 NSStatusItem");
            return;
        };
        let menu = build_menu(mtm, &data);
        status_item.setMenu(Some(&menu));
        logging!(debug, Type::Tray, "[native] 原生托盘菜单已刷新");
    });
    if let Err(e) = result {
        logging!(warn, Type::Tray, "[native] with_inner_tray_icon 失败: {}", e);
    }
}
