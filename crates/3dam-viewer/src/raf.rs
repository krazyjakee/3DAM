//! A self-rescheduling `requestAnimationFrame` loop that the island owns and the `Drop` cancels.
//!
//! The island drives its own frames (it "owns the pixels", tech-spec 09 §B.3) so the DOM only has
//! to call `set_camera`/`resize`/`load_model` and mark state dirty. The classic wasm-bindgen rAF
//! pattern leaks its closure via an `Rc` cycle; [`RafHandle::drop`] breaks that cycle so tearing an
//! island down (React unmount → `free()`) actually stops the loop and frees GPU state.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// Shared, mutable slot holding the rAF callback — cleared on teardown to break the retain cycle.
type FrameCb = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;

/// Handle whose lifetime *is* the loop's lifetime. Drop it → the loop stops on the next tick and the
/// closure (with everything it captured) is released.
pub struct RafHandle {
    running: Rc<Cell<bool>>,
    // Same allocation the running closure holds a clone of — taking the `Option` here breaks the
    // `Rc<RefCell<Closure>> → Closure → Rc` cycle that would otherwise leak.
    closure: FrameCb,
}

impl Drop for RafHandle {
    fn drop(&mut self) {
        self.running.set(false);
        let _ = self.closure.borrow_mut().take();
    }
}

fn request_frame(cb: &Closure<dyn FnMut()>) {
    if let Some(win) = web_sys::window() {
        let _ = win.request_animation_frame(cb.as_ref().unchecked_ref());
    }
}

/// Start an rAF loop calling `tick` once per frame until the returned handle is dropped.
pub fn start(mut tick: impl FnMut() + 'static) -> RafHandle {
    let running = Rc::new(Cell::new(true));
    let closure: FrameCb = Rc::new(RefCell::new(None));

    let running_inner = running.clone();
    let closure_inner = closure.clone();
    *closure.borrow_mut() = Some(Closure::wrap(Box::new(move || {
        if !running_inner.get() {
            return;
        }
        tick();
        if let Some(cb) = closure_inner.borrow().as_ref() {
            request_frame(cb);
        }
    }) as Box<dyn FnMut()>));

    if let Some(cb) = closure.borrow().as_ref() {
        request_frame(cb);
    }

    RafHandle { running, closure }
}
