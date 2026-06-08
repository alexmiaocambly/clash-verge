//! macOS 托盘速率显示模块（自绘 NSView 方案，对齐 ClashX）
//!
//! AppKit 在「button.image + attributedTitle」结构下无法控制图标与文字的间距，
//! 因此改用 ClashX 同款做法：让按钮不自绘内容（`imagePosition = .imageOverlaps`），
//! 在按钮上叠加一个自定义 `NSView`，由它自绘「图标 + 两行网速」，间距/对齐用布局常量精确控制。
//! 该 view 的 `hitTest` 返回 nil 让点击穿透，保留 tray-icon 原有的点击/菜单逻辑。

use std::cell::RefCell;

use crate::utils::speed::format_bytes_per_second;
use crate::{Type, logging};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread as _, DefinedClass as _, MainThreadMarker, MainThreadOnly as _, define_class, msg_send};
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing as _, NSBezierPath, NSCellImagePosition, NSColor, NSCompositingOperation, NSFont,
    NSFontAttributeName, NSFontWeightRegular, NSForegroundColorAttributeName, NSImage, NSLineBreakMode,
    NSMutableParagraphStyle, NSParagraphStyleAttributeName, NSStatusBarButton, NSStatusItem, NSTextAlignment, NSView,
};
use objc2_foundation::{NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString};

/// 网速字号
const TRAY_FONT_SIZE: f64 = 9.0;
/// 图标绘制尺寸
const TRAY_ICON_SIZE: f64 = 18.0;
/// 图标左侧留白
const TRAY_ICON_LEFT: f64 = 1.0;
/// 速率显示时状态项的固定总宽度。固定宽度让图标不随网速宽窄左右跳动；
/// 网速右对齐贴在右侧 —— **调大本值 = 图标与网速间距变大，调小 = 间距变小**。
const TRAY_STATUS_ITEM_FIXED_LENGTH: f64 = 60.0;
/// 网速右对齐后到状态项右边缘的留白
const TRAY_TEXT_RIGHT_PAD: f64 = 3.0;
/// AppKit 的 NSVariableStatusItemLength，清除速率显示后恢复按图标自适应
const NS_VARIABLE_STATUS_ITEM_LENGTH: f64 = -1.0;

/// 将上行/下行速率格式化为两行文本（上行在上，下行在下）。
/// 不做前导空格补齐——两行的右边缘对齐由段落右对齐 + 文字块宽度=实际宽度实现。
fn format_tray_speed(up: u64, down: u64) -> String {
    format!("{}\n{}", format_bytes_per_second(up), format_bytes_per_second(down))
}

/// 构建两行网速的富文本（数字等宽字体、自适应标签色、右对齐）
fn build_speed_attributed(text: &str) -> Retained<NSAttributedString> {
    let attrs: Retained<NSDictionary<NSString, AnyObject>> = unsafe {
        let font = NSFont::monospacedDigitSystemFontOfSize_weight(TRAY_FONT_SIZE, NSFontWeightRegular);
        let color = NSColor::labelColor();
        let para = NSMutableParagraphStyle::new();
        // 右对齐：两行数字右边缘对齐。文字块整体位置由绘制时的 rect 控制（rect 宽=文字实际宽，紧贴图标）
        para.setAlignment(NSTextAlignment::Right);
        para.setLineBreakMode(NSLineBreakMode::ByClipping);
        let keys: &[&NSString] = &[
            NSFontAttributeName,
            NSForegroundColorAttributeName,
            NSParagraphStyleAttributeName,
        ];
        let values: &[&AnyObject] = &[&font, &color, &para];
        NSDictionary::from_slices(keys, values)
    };
    unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(&attrs),
        )
    }
}

struct SpeedViewIvars {
    text: RefCell<String>,
    icon: RefCell<Option<Retained<NSImage>>>,
}

/// 把 template（单色自适应）图标按当前外观染成给定颜色，使其跟随深色/浅色模式（否则自绘会画出原始像素、颜色不对）
#[allow(deprecated)]
fn tint_template(img: &NSImage, color: &NSColor) -> Retained<NSImage> {
    let size = img.size();
    let out = NSImage::initWithSize(NSImage::alloc(), size);
    let rect = NSRect::new(NSPoint::new(0.0, 0.0), size);
    out.lockFocus();
    color.set();
    NSBezierPath::bezierPathWithRect(rect).fill();
    // 用图标 alpha 作蒙版，只保留颜色在图标形状内（DestinationIn）
    img.drawInRect_fromRect_operation_fraction(
        rect,
        NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
        NSCompositingOperation::DestinationIn,
        1.0,
    );
    out.unlockFocus();
    out
}

define_class!(
    #[unsafe(super(NSView))]
    #[name = "ClashVergeSpeedView"]
    #[ivars = SpeedViewIvars]
    struct SpeedView;

    impl SpeedView {
        /// 翻转坐标系，使用左上角原点，便于自上而下布局
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// 自绘：左侧图标（垂直居中）+ 右侧两行网速（右对齐、垂直居中）
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let bounds = self.bounds();
            let ivars = self.ivars();

            // 图标
            if let Some(img) = ivars.icon.borrow().as_ref() {
                let y = (bounds.size.height - TRAY_ICON_SIZE) / 2.0;
                let rect = NSRect::new(NSPoint::new(TRAY_ICON_LEFT, y), NSSize::new(TRAY_ICON_SIZE, TRAY_ICON_SIZE));
                let zero = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0));
                // template 图标需按当前外观染色（跟随深色/浅色），彩色图标直接画
                if img.isTemplate() {
                    let tinted = tint_template(img, &NSColor::labelColor());
                    tinted.drawInRect_fromRect_operation_fraction(rect, zero, NSCompositingOperation::SourceOver, 1.0);
                } else {
                    img.drawInRect_fromRect_operation_fraction(rect, zero, NSCompositingOperation::SourceOver, 1.0);
                }
            }

            // 网速文字：rect 从图标右延伸到状态项右边缘，段落右对齐使网速贴在最右；
            // 图标-网速间距 = 固定宽度 − 图标 − 网速宽，由 TRAY_STATUS_ITEM_FIXED_LENGTH 调节
            let text = ivars.text.borrow();
            if !text.is_empty() {
                let attr = build_speed_attributed(&text);
                let text_h = attr.size().height;
                let text_left = TRAY_ICON_LEFT + TRAY_ICON_SIZE;
                let text_w = (bounds.size.width - text_left - TRAY_TEXT_RIGHT_PAD).max(0.0);
                let y = (bounds.size.height - text_h) / 2.0;
                let rect = NSRect::new(NSPoint::new(text_left, y), NSSize::new(text_w, text_h));
                attr.drawInRect(rect);
            }
        }

        /// 点击穿透：让 tray-icon 的命中视图收到点击，保留原有点击/菜单逻辑
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

impl SpeedView {
    fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SpeedViewIvars {
            text: RefCell::new(String::new()),
            icon: RefCell::new(None),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

/// 找到按钮上已存在的 SpeedView，没有则创建并添加
fn ensure_speed_view(button: &NSStatusBarButton, mtm: MainThreadMarker) -> Retained<SpeedView> {
    let subviews = button.subviews();
    for i in 0..subviews.count() {
        if let Ok(view) = subviews.objectAtIndex(i).downcast::<SpeedView>() {
            return view;
        }
    }
    let view = SpeedView::new(mtm, button.bounds());
    button.addSubview(&view);
    view
}

/// 移除按钮上的 SpeedView
fn remove_speed_view(button: &NSStatusBarButton) {
    let subviews = button.subviews();
    for i in 0..subviews.count() {
        if let Ok(view) = subviews.objectAtIndex(i).downcast::<SpeedView>() {
            view.removeFromSuperview();
        }
    }
}

/// 以自绘 view 显示速率（图标 + 两行网速）。依赖 `with_inner_tray_icon` 保证主线程调用。
pub fn set_speed_attributed_title(status_item: &NSStatusItem, up: u64, down: u64) {
    let Some(mtm) = MainThreadMarker::new() else {
        logging!(warn, Type::Tray, "托盘速率显示跳过：非主线程调用");
        return;
    };
    let Some(button) = status_item.button(mtm) else {
        return;
    };

    // 固定宽度（图标不跳）；网速在绘制时右对齐贴右，图标-网速间距由该固定宽度决定
    let text = format_tray_speed(up, down);
    status_item.setLength(TRAY_STATUS_ITEM_FIXED_LENGTH);

    // 先建好 SpeedView 并填好图标/网速内容，再让按钮停止自绘，避免出现“按钮不画 + view 未画”的空档
    let view = ensure_speed_view(&button, mtm);
    view.setFrame(button.bounds());
    let ivars = view.ivars();
    *ivars.text.borrow_mut() = text;
    // 从按钮当前图标取图，交由 view 绘制（与 tray-icon 的图标切换保持同步）
    *ivars.icon.borrow_mut() = button.image();
    view.setNeedsDisplay(true);

    // SpeedView 已就绪，让按钮不再自绘图标（NoImage 保留 button.image 供 SpeedView 读取，避免双图标）
    button.setImagePosition(NSCellImagePosition::NoImage);
}

/// 图标更新后调用：tray-icon 的 set_icon 会把按钮 imagePosition 重设为 ImageLeft（按钮重新自绘图标，
/// 与 SpeedView 的图标重复成双图标）。这里把它重新设回 NoImage，并把新图标同步给 SpeedView。
pub fn reassert_speed_icon(status_item: &NSStatusItem) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(button) = status_item.button(mtm) else {
        return;
    };
    let subviews = button.subviews();
    let mut found = false;
    for i in 0..subviews.count() {
        if let Ok(view) = subviews.objectAtIndex(i).downcast::<SpeedView>() {
            *view.ivars().icon.borrow_mut() = button.image();
            view.setNeedsDisplay(true);
            found = true;
        }
    }
    // 仅当 SpeedView 已存在（已能接管绘制）时才让按钮停止自绘图标，
    // 否则速率任务尚未首次刷新、view 还没建立，会出现“按钮不画 + 无 view”的图标空档。
    if found {
        button.setImagePosition(NSCellImagePosition::NoImage);
    }
}

/// 清除速率显示，恢复按钮自绘图标
pub fn clear_speed_attributed_title(status_item: &NSStatusItem) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(button) = status_item.button(mtm) else {
        return;
    };
    remove_speed_view(&button);
    button.setImagePosition(NSCellImagePosition::ImageLeft);
    status_item.setLength(NS_VARIABLE_STATUS_ITEM_LENGTH);
}
