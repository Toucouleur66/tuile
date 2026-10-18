// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the viewer asks of the desktop it runs on: receive steering — URLs,
//! and a script's questions and commands — show a message when there is no
//! terminal to print it in, and put a line of text on the clipboard.
//!
//! The rest of the viewer sees plain functions. The implementation that talks
//! to an operating system is the private `system` module, compiled on macOS
//! and iOS with the `application` feature; in a browser the same services are
//! the page's, through the web shell — messages and the view become events for
//! the host's script; everywhere else nothing ever arrives, the message goes
//! to stderr and the clipboard declines.
//!
//! **Exercised by hand only**, like the location service: nothing here can be
//! called from a test without a window server and a person. What a URL
//! *means* is tested in [`crate::steer`], what a property *answers* in
//! [`crate::snapshot`]; this module only carries strings and numbers across.
//! `macos/scripting-check.sh` runs the real thing against an installed bundle.

use std::sync::mpsc::Receiver;

use crate::snapshot::Snapshot;
use crate::steer::Request;

/// Starts listening for steering and returns where it will arrive.
///
/// Call once, on the main thread, before the event loop starts: a URL that
/// *launched* the application is delivered as the loop begins, and a listener
/// installed later would miss it. Nothing that arrives is trusted — a URL is
/// only a string until [`crate::steer::parse_url`] has read it, a script's
/// numbers are held to the flags' bounds before they become a command — and
/// everything is only queued here: the handlers run inside the system's event
/// dispatch, and the camera is moved by the render loop when it drains the
/// queue. A script's *questions* never reach the loop at all; they are
/// answered from [`crate::snapshot::current`].
pub(crate) fn listen() -> Receiver<Request> {
    system::listen()
}

/// Shows a message to someone who may have no terminal, and waits for them to
/// dismiss it.
pub(crate) fn alert(title: &str, text: &str) {
    system::alert(title, text);
}

/// Shows a message if it can be shown now, and says whether it was.
///
/// A phone presents a message over its window, and a presentation asked for
/// before that window is on screen is dropped without a word — so the caller
/// keeps the message and asks again on a later turn. Everywhere else this is
/// [`alert`], and always shown.
pub(crate) fn try_alert(title: &str, text: &str) -> bool {
    system::try_alert(title, text)
}

/// Shows a line of text over the bottom of the window, where there is no title
/// bar to carry it: whose pictures are on screen, and the last thing said.
/// Nothing where the window has a title bar, which already says it.
pub(crate) fn caption(text: &str) {
    system::caption(text);
}

/// Puts `text` on the clipboard; `false` when there is no clipboard to put it
/// on.
pub(crate) fn copy(text: &str) -> bool {
    system::copy(text)
}

/// What a paste found.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Pasted {
    /// The text on the clipboard, if it holds text.
    Now(Option<String>),
    /// Nothing yet: a page's clipboard answers later, once the browser has
    /// asked the person, and the text arrives on the steering queue as
    /// [`Request::Pasted`].
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    Later,
}

/// The text on the clipboard. Read only when the person asks for it — "go to
/// copied link" — and only looked at as a steering URL.
pub(crate) fn paste() -> Pasted {
    system::paste()
}

/// Tells whoever embeds the viewer — a page — that the window draws, and
/// through which graphics backend; or that it never will, and why. An
/// application that is the whole window has nobody to tell, here and in the
/// two below: it says a failure in its own words, through an alert.
pub(crate) fn drawing(outcome: Result<&str, &str>) {
    system::drawing(outcome);
}

/// Tells the embedder whether the loading indicator is up, every frame, and
/// how far the launch has got when that is known.
pub(crate) fn loading(shown: bool, progress: Option<f32>) {
    system::loading(shown, progress);
}

/// Tells the embedder the view, whenever the title line changes: a few times
/// a second at most while the eye moves.
pub(crate) fn view(snapshot: &Snapshot) {
    system::view(snapshot);
}

/// Puts a request on the queue the render loop drains: the one way anything
/// outside the loop — the system's event dispatch, a menu — acts on the view.
#[cfg_attr(
    not(all(target_os = "macos", feature = "application")),
    allow(dead_code)
)]
pub(crate) fn queue(request: Request) {
    system::send(request);
}

#[cfg(all(target_os = "macos", feature = "application"))]
mod system {
    #![allow(unsafe_code)]

    use std::ffi::CString;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Mutex;

    use objc2::ffi::class_addMethod;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Imp, NSObject, Sel};
    use objc2::{define_class, msg_send, sel, AllocAnyThread, ClassType, MainThreadMarker};
    use objc2_app_kit::{
        NSAlert, NSApplication, NSApplicationActivationPolicy, NSPasteboard, NSPasteboardTypeString,
    };
    use objc2_foundation::{
        NSAppleEventDescriptor, NSAppleEventManager, NSNumber, NSScriptCommand, NSString,
    };

    use crate::snapshot::{self, Value, PROPERTIES};
    use crate::steer::{Command, Goto, Request};

    /// `'GURL'`: both the class and the id of the "open this URL" Apple event.
    const GET_URL: u32 = u32::from_be_bytes(*b"GURL");
    /// `'----'`: the keyword of an Apple event's direct parameter — here, the
    /// URL itself.
    const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
    /// The event ids of the dictionary's three commands, as `Tuile.sdef`
    /// spells them after the suite's `Tuil`.
    const GO_TO: u32 = u32::from_be_bytes(*b"goto");
    const NORTH_UP: u32 = u32::from_be_bytes(*b"nrth");
    const HERE: u32 = u32::from_be_bytes(*b"here");
    /// The system's "a parameter is wrong" error number.
    const PARAMETER_ERROR: isize = -50;

    /// Where every handler below leaves what it received. One queue, because
    /// the render loop has one place to look.
    static OUTBOX: Mutex<Option<Sender<Request>>> = Mutex::new(None);

    pub(super) fn send(request: Request) {
        if let Ok(outbox) = OUTBOX.lock() {
            if let Some(sender) = outbox.as_ref() {
                // A closed channel means the window is gone; so is the point
                // of steering it.
                let _ = sender.send(request);
            }
        }
    }

    pub(super) fn drawing(_outcome: Result<&str, &str>) {}

    pub(super) fn loading(_shown: bool, _progress: Option<f32>) {}

    pub(super) fn view(_snapshot: &crate::snapshot::Snapshot) {}

    define_class!(
        // SAFETY: `NSObject` has no subclassing requirements, and the class
        // does not implement `Drop`.
        #[unsafe(super(NSObject))]
        #[name = "TuileViewerUrlHandler"]
        struct UrlHandler;

        impl UrlHandler {
            /// The signature the event manager calls: the event, and a reply
            /// this handler leaves empty.
            #[unsafe(method(handleURL:withReply:))]
            fn handle(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
                // SAFETY: `paramDescriptorForKeyword:` takes a four-character
                // code and returns a descriptor or nil.
                let url: Option<Retained<NSAppleEventDescriptor>> =
                    unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
                if let Some(url) = url.and_then(|d| d.stringValue()) {
                    send(Request::Url(url.to_string()));
                }
            }
        }
    );

    define_class!(
        // SAFETY: `NSScriptCommand` is meant to be subclassed, with exactly
        // this method overridden; the class does not implement `Drop`.
        //
        // One class for the three commands of the dictionary: the scripting
        // runtime instantiates it by name and the event's own code says which
        // command it is.
        #[unsafe(super(NSScriptCommand))]
        #[name = "TuileViewerScriptCommand"]
        struct ScriptCommand;

        impl ScriptCommand {
            #[unsafe(method_id(performDefaultImplementation))]
            fn perform(&self) -> Option<Retained<AnyObject>> {
                let description = self.commandDescription();
                // SAFETY: a plain property read returning a four-character
                // code.
                let code: u32 = unsafe { msg_send![&*description, appleEventCode] };
                let command = match code {
                    NORTH_UP => Ok(Command::NorthUp),
                    HERE => Ok(Command::Here),
                    GO_TO => self.goto().map(Command::Goto),
                    _ => Err("not a command of this application".to_owned()),
                };
                match command {
                    Ok(command) => send(Request::Do(command)),
                    // The script is told, in its own terms: an error number
                    // and the sentence the flags would have printed.
                    Err(why) => {
                        self.setScriptErrorNumber(PARAMETER_ERROR);
                        self.setScriptErrorString(Some(&NSString::from_str(&why)));
                    }
                }
                None
            }
        }
    );

    impl ScriptCommand {
        /// The `go to` this event carries, its numbers held to the flags'
        /// bounds. The argument names are the dictionary's `cocoa key`s.
        fn goto(&self) -> Result<Goto, String> {
            let arguments = self.evaluatedArguments();
            let mut given = Vec::new();
            for name in ["lon", "lat", "altitude", "heading", "pitch"] {
                let value = arguments
                    .as_ref()
                    .and_then(|a| a.objectForKey(&NSString::from_str(name)));
                if let Some(value) = value {
                    // SAFETY: the dictionary types these parameters `real`,
                    // so the runtime hands over numbers.
                    let number: f64 = unsafe { msg_send![&*value, doubleValue] };
                    given.push((name, number));
                }
            }
            Goto::from_numbers(&given)
        }
    }

    /// Answers one property of the application object: the selector *is* the
    /// key. Every answer is an object — a number or a string — or nil, which a
    /// script reads as `missing value`.
    extern "C-unwind" fn get(_app: &AnyObject, key: Sel) -> *mut AnyObject {
        let answer = key
            .name()
            .to_str()
            .ok()
            .zip(snapshot::current())
            .and_then(|(key, view)| snapshot::property(&view, key));
        match answer {
            Some(Value::Real(x)) => Retained::autorelease_ptr(NSNumber::new_f64(x)).cast(),
            Some(Value::Integer(n)) => Retained::autorelease_ptr(NSNumber::new_i64(n)).cast(),
            Some(Value::Flag(on)) => Retained::autorelease_ptr(NSNumber::new_bool(on)).cast(),
            Some(Value::Text(text)) => Retained::autorelease_ptr(NSString::from_str(&text)).cast(),
            Some(Value::Withheld | Value::Missing) | None => std::ptr::null_mut(),
        }
    }

    /// Takes a script's `set wireframe to …` / `set frozen to …`: the selector
    /// says which, the value is a number, and what comes of it is a command on
    /// the queue like any other.
    extern "C-unwind" fn set(_app: &AnyObject, setter: Sel, value: *mut AnyObject) {
        if value.is_null() {
            return;
        }
        if setter.name().to_bytes() == b"setImagery:" {
            // SAFETY: the dictionary types this property `text`, so the
            // runtime hands over a string; `description` is defined on every
            // object and returns one whatever it was handed.
            let name: Retained<NSString> = unsafe { msg_send![value, description] };
            // A name and nothing else: which layer it is, if any, is decided
            // by the render loop against the host's list.
            send(Request::Imagery(name.to_string()));
            return;
        }
        // SAFETY: the dictionary types both properties `boolean`, so the
        // runtime hands over a number; `boolValue` is a plain read.
        let on: bool = unsafe { msg_send![value, boolValue] };
        match setter.name().to_bytes() {
            b"setWireframe:" => send(Request::Do(Command::Wireframe(on))),
            b"setFrozen:" => send(Request::Do(Command::Freeze(on))),
            _ => {}
        }
    }

    /// Gives the application object the dictionary's properties.
    ///
    /// The scripting runtime reads a property of `application` by key-value
    /// coding on the shared application object, which means: by calling a
    /// method named like the key. The windowing layer owns that object's class
    /// and its delegate, so the methods are added to the class at run time —
    /// what a category does in the system's own language — one per key of
    /// [`PROPERTIES`], all pointing at the two functions above.
    fn add_the_properties() {
        let class = (NSApplication::class() as *const AnyClass).cast_mut();
        for (key, _, writable) in PROPERTIES {
            let Ok(name) = CString::new(key) else {
                continue;
            };
            // SAFETY: `get` has the signature its type string states — an
            // object returned, from `self` and `_cmd` — and the class is the
            // live `NSApplication` class.
            unsafe {
                let imp: Imp = std::mem::transmute::<
                    extern "C-unwind" fn(&AnyObject, Sel) -> *mut AnyObject,
                    Imp,
                >(get);
                class_addMethod(class, Sel::register(&name), imp, c"@@:".as_ptr());
            }
            if !writable {
                continue;
            }
            let mut capital = key.to_owned();
            capital[..1].make_ascii_uppercase();
            let Ok(name) = CString::new(format!("set{capital}:")) else {
                continue;
            };
            // SAFETY: as above, for a method taking one object and returning
            // nothing.
            unsafe {
                let imp: Imp = std::mem::transmute::<
                    extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject),
                    Imp,
                >(set);
                class_addMethod(class, Sel::register(&name), imp, c"v@:@".as_ptr());
            }
        }
    }

    pub(super) fn listen() -> Receiver<Request> {
        let (sender, receiver) = channel();
        if let Ok(mut outbox) = OUTBOX.lock() {
            *outbox = Some(sender);
        }
        let handler = UrlHandler::alloc();
        // SAFETY: `NSObject`'s `init` is the designated initialiser.
        let handler: Retained<UrlHandler> = unsafe { msg_send![handler, init] };
        let manager = NSAppleEventManager::sharedAppleEventManager();
        // SAFETY: the selector exists on the handler with the signature the
        // manager calls, and the two codes are plain `u32`s.
        let _: () = unsafe {
            msg_send![
                &*manager,
                setEventHandler: &*handler,
                andSelector: sel!(handleURL:withReply:),
                forEventClass: GET_URL,
                andEventID: GET_URL,
            ]
        };
        // The manager does not keep its handlers alive, and this one is wanted
        // for as long as the process runs.
        std::mem::forget(handler);
        // The scripting runtime finds the command class by the name the
        // dictionary gives; it has to exist before the first script asks.
        let _ = ScriptCommand::class();
        add_the_properties();
        receiver
    }

    pub(super) fn alert(title: &str, text: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            eprintln!("{title}\n\n{text}");
            return;
        };
        // An alert from a process that is not yet an application appears
        // behind everything, or not at all: become one, and come forward.
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        alert.runModal();
    }

    pub(super) fn try_alert(title: &str, text: &str) -> bool {
        alert(title, text);
        true
    }

    /// The title bar says it.
    pub(super) fn caption(_text: &str) {}

    pub(super) fn copy(text: &str) -> bool {
        let board = NSPasteboard::generalPasteboard();
        board.clearContents();
        // SAFETY: the string type is a constant the framework exports.
        board.setString_forType(&NSString::from_str(text), unsafe { NSPasteboardTypeString })
    }

    pub(super) fn paste() -> super::Pasted {
        let board = NSPasteboard::generalPasteboard();
        // SAFETY: the string type is a constant the framework exports.
        super::Pasted::Now(
            board
                .stringForType(unsafe { NSPasteboardTypeString })
                .map(|text| text.to_string()),
        )
    }
}

/// A phone: a message is an alert over the window, and the caption a label
/// along its bottom edge. Neither blocks — a phone's main thread may not wait
/// for a person — and both are dropped, with a line on stderr, when there is
/// no window yet to show them over.
#[cfg(all(target_os = "ios", feature = "application"))]
mod system {
    #![allow(unsafe_code)]

    use std::cell::RefCell;
    use std::sync::mpsc::{channel, Receiver};

    use objc2::rc::Retained;
    use objc2::{ClassType, MainThreadMarker};
    use objc2_foundation::NSString;
    use objc2_ui_kit::{
        UIAlertAction, UIAlertActionStyle, UIAlertController, UIAlertControllerStyle,
        UIApplication, UIColor, UIFont, UILabel, UIPasteboard, UIView, UIViewAutoresizing,
        UIViewController,
    };

    use crate::steer::Request;

    thread_local! {
        /// The caption's label, once made. Main thread only, as every view.
        static CAPTION: RefCell<Option<Retained<UILabel>>> = const { RefCell::new(None) };
    }

    pub(super) fn listen() -> Receiver<Request> {
        // Nothing steers a phone's viewer from outside yet: the queue is
        // always empty.
        channel().1
    }

    /// The window's root view controller: what an alert is presented on, and
    /// whose view the caption is laid over.
    fn root(mtm: MainThreadMarker) -> Option<Retained<UIViewController>> {
        let _ = mtm;
        // SAFETY: a class method with no arguments, on the main thread. Asked
        // for as an `Option` rather than through the binding, which panics on
        // the null it returns before the system's loop has started.
        let app: Option<Retained<UIApplication>> =
            unsafe { objc2::msg_send![UIApplication::class(), sharedApplication] };
        let app = app?;
        #[allow(deprecated)]
        let window = app.keyWindow()?;
        window.rootViewController()
    }

    pub(super) fn alert(title: &str, text: &str) {
        if !try_alert(title, text) {
            eprintln!("{title}\n\n{text}");
        }
    }

    /// Presents the message over the window's root, if that root's view is
    /// on screen and showing nothing else, and says whether it took.
    pub(super) fn try_alert(title: &str, text: &str) -> bool {
        let Some(mtm) = MainThreadMarker::new() else {
            return false;
        };
        let Some(root) = root(mtm) else {
            return false;
        };
        let on_screen = root.isViewLoaded() && root.view().and_then(|v| v.window()).is_some();
        if !on_screen || root.presentedViewController().is_some() {
            return false;
        }
        let controller = UIAlertController::alertControllerWithTitle_message_preferredStyle(
            Some(&NSString::from_str(title)),
            Some(&NSString::from_str(text)),
            UIAlertControllerStyle::Alert,
            mtm,
        );
        // A nil handler: the button only dismisses the alert.
        let ok = UIAlertAction::actionWithTitle_style_handler(
            Some(&NSString::from_str("OK")),
            UIAlertActionStyle::Default,
            None,
            mtm,
        );
        controller.addAction(&ok);
        // On the window's root, on the main thread; no completion.
        root.presentViewController_animated_completion(&controller, true, None);
        // Dropped presentations leave nothing presented.
        root.presentedViewController().is_some()
    }

    pub(super) fn caption(text: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let Some(view) = root(mtm).and_then(|root| root.view()) else {
            return;
        };
        CAPTION.with(|cell| {
            let mut cell = cell.borrow_mut();
            let label = cell.get_or_insert_with(|| make_label(mtm, &view));
            label.setText(Some(&NSString::from_str(text)));
            place(label, &view);
            // Over the drawable: a surface made again after the background
            // puts its layer on top of everything already in the view.
            view.bringSubviewToFront(label);
        });
    }

    fn make_label(mtm: MainThreadMarker, view: &UIView) -> Retained<UILabel> {
        let label = UILabel::new(mtm);
        // SAFETY: plain property writes on a label made on this thread, with
        // objects of the types the properties hold.
        unsafe {
            label.setFont(Some(&UIFont::systemFontOfSize(11.0)));
            label.setTextColor(Some(&UIColor::colorWithWhite_alpha(1.0, 0.85)));
        }
        label.setShadowColor(Some(&UIColor::colorWithWhite_alpha(0.0, 0.6)));
        label.setUserInteractionEnabled(false);
        // Follows the bottom edge and the width through a rotation.
        label.setAutoresizingMask(
            UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleTopMargin,
        );
        view.addSubview(&label);
        label
    }

    /// Along the bottom of the safe area, clear of the home indicator.
    fn place(label: &UILabel, view: &UIView) {
        let bounds = view.bounds();
        let insets = view.safeAreaInsets();
        let height = 16.0;
        let frame = objc2_core_foundation::CGRect::new(
            objc2_core_foundation::CGPoint::new(
                insets.left + 12.0,
                bounds.size.height - insets.bottom - height - 4.0,
            ),
            objc2_core_foundation::CGSize::new(
                (bounds.size.width - insets.left - insets.right - 24.0).max(0.0),
                height,
            ),
        );
        label.setFrame(frame);
    }

    pub(super) fn copy(text: &str) -> bool {
        MainThreadMarker::new().is_some_and(|_| {
            // SAFETY: a property write of a string, on the main thread.
            unsafe { UIPasteboard::generalPasteboard().setString(Some(&NSString::from_str(text))) };
            true
        })
    }

    pub(super) fn paste() -> super::Pasted {
        if MainThreadMarker::new().is_none() {
            return super::Pasted::Now(None);
        }
        // SAFETY: a property read, on the main thread.
        super::Pasted::Now(
            unsafe { UIPasteboard::generalPasteboard().string() }.map(|text| text.to_string()),
        )
    }

    pub(super) fn send(_request: Request) {}

    pub(super) fn drawing(_outcome: Result<&str, &str>) {}

    pub(super) fn loading(_shown: bool, _progress: Option<f32>) {}

    pub(super) fn view(_snapshot: &crate::snapshot::Snapshot) {}
}

/// A page: everything goes to the host's script through the web shell.
#[cfg(target_arch = "wasm32")]
mod system {
    pub(super) use crate::web::system::{
        alert, caption, copy, drawing, listen, loading, paste, send, try_alert, view,
    };
}

#[cfg(not(any(
    all(target_os = "macos", feature = "application"),
    all(target_os = "ios", feature = "application"),
    target_arch = "wasm32"
)))]
mod system {
    use std::sync::mpsc::{channel, Receiver};

    use crate::steer::Request;

    pub(super) fn listen() -> Receiver<Request> {
        // The sender is dropped here: the queue is simply always empty.
        channel().1
    }

    pub(super) fn alert(title: &str, text: &str) {
        eprintln!("{title}\n\n{text}");
    }

    pub(super) fn try_alert(title: &str, text: &str) -> bool {
        alert(title, text);
        true
    }

    pub(super) fn caption(_text: &str) {}

    pub(super) fn copy(_text: &str) -> bool {
        false
    }

    pub(super) fn paste() -> super::Pasted {
        super::Pasted::Now(None)
    }

    pub(super) fn send(_request: Request) {}

    pub(super) fn drawing(_outcome: Result<&str, &str>) {}

    pub(super) fn loading(_shown: bool, _progress: Option<f32>) {}

    pub(super) fn view(_snapshot: &crate::snapshot::Snapshot) {}
}
