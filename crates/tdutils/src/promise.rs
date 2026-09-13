//! Port of `td/utils/Promise.h`: the promise machinery — `PromiseInterface`,
//! `Promise`, `LambdaPromise`, `SafePromise` and `PromiseCreator`.
//!
//! # Mapping notes
//!
//! - C++ `td::Unit` maps to `()` (the crate has no `Unit` type).
//! - C++ `PromiseCreator::lambda` auto-detects whether the callable takes the
//!   value or a `Result<T>`; Rust has no function-trait specialization, so
//!   [`PromiseCreator::lambda`] is the value form and
//!   [`PromiseCreator::lambda_result`] the result form.
//! - The value form requires `T: Default` exactly like C++: the error path
//!   calls the handler with `Auto()`, i.e. a default-constructed `T`.
//! - `CancellationToken` (`is_cancellable`/`is_canceled`) is not ported yet;
//!   the base implementations return `false`, as in the C++ base class.

use crate::status::{Result, Status};

/// The port of C++ `PromiseInterface<T>`: the object behind a [`Promise`].
///
/// C++ passes `T` by rvalue reference; Rust takes it by value, which is the
/// same move.
pub trait PromiseInterface<T: Send>: Send {
    /// C++ `set_value(T &&)`.
    fn set_value(&mut self, value: T);

    /// C++ `set_error(Status &&)`.
    fn set_error(&mut self, error: Status);

    /// C++ `set_result(Result<T> &&)` — dispatches to `set_value`/`set_error`.
    fn set_result(&mut self, result: Result<T>) {
        match result {
            Result::Ok(value) => self.set_value(value),
            Result::Err(status) => self.set_error(status),
        }
    }

    /// C++ `is_cancellable()` (the `CancellationToken` part is not ported).
    fn is_cancellable(&self) -> bool {
        false
    }

    /// C++ `is_canceled()`.
    fn is_canceled(&self) -> bool {
        false
    }
}

/// The port of C++ `Promise<T>`: a possibly-empty holder of a
/// [`PromiseInterface`]. Every `set_*` consumes the interface — the promise is
/// one-shot. Dropping an unfulfilled promise is silent here; the lost-promise
/// error is emitted by the concrete interfaces that define it
/// ([`LambdaPromise`], the actor-side `PromiseActor` sends `hangup`).
pub struct Promise<T: Send> {
    inner: Option<Box<dyn PromiseInterface<T>>>,
}

impl<T: Send> Promise<T> {
    /// C++ `Promise(unique_ptr<PromiseInterface<T>>)`.
    pub fn from_interface(interface: impl PromiseInterface<T> + 'static) -> Self {
        Self {
            inner: Some(Box::new(interface)),
        }
    }

    /// C++ `set_value`: calls the interface and drops it.
    pub fn set_value(&mut self, value: T) {
        if let Some(mut inner) = self.inner.take() {
            inner.set_value(value);
        }
    }

    /// C++ `set_error(Status &&)`.
    pub fn set_error(&mut self, error: Status) {
        if let Some(mut inner) = self.inner.take() {
            inner.set_error(error);
        }
    }

    /// C++ `set_result(Result<T> &&)`.
    pub fn set_result(&mut self, result: Result<T>) {
        if let Some(mut inner) = self.inner.take() {
            inner.set_result(result);
        }
    }

    /// C++ `reset()`.
    pub fn reset(&mut self) {
        self.inner = None;
    }

    /// C++ `explicit operator bool` — whether the promise still holds an
    /// interface.
    pub fn is_empty(&self) -> bool {
        self.inner.is_none()
    }

    /// C++ `is_cancellable()`.
    pub fn is_cancellable(&self) -> bool {
        self.inner.as_ref().is_some_and(|p| p.is_cancellable())
    }

    /// C++ `is_canceled()`.
    pub fn is_canceled(&self) -> bool {
        self.inner.as_ref().is_some_and(|p| p.is_canceled())
    }

    /// C++ `release()` — take the interface out.
    pub fn release(&mut self) -> Option<Box<dyn PromiseInterface<T>>> {
        self.inner.take()
    }
}

impl<T: Send> Default for Promise<T> {
    fn default() -> Self {
        Self { inner: None }
    }
}

/// The port of C++ `detail::LambdaPromise`: a [`PromiseInterface`] backed by a
/// closure. The closure is stored as `FnOnce(Result<T>)`; the value form wraps
/// the error path as `f(T::default())` (the C++ `func_(Auto())`).
///
/// Dropping it while still unfulfilled calls the handler with the C++
/// `"Lost promise"` error.
pub struct LambdaPromise<T: Send> {
    func: Option<Box<dyn FnOnce(Result<T>) + Send>>,
}

impl<T: Send> LambdaPromise<T> {
    fn from_value_handler(f: impl FnOnce(T) + Send + 'static) -> Self
    where
        T: Default,
    {
        Self {
            func: Some(Box::new(move |result| match result {
                Result::Ok(value) => f(value),
                Result::Err(_) => f(T::default()),
            })),
        }
    }

    fn from_result_handler(f: impl FnOnce(Result<T>) + Send + 'static) -> Self {
        Self {
            func: Some(Box::new(f)),
        }
    }
}

impl<T: Send> PromiseInterface<T> for LambdaPromise<T> {
    fn set_value(&mut self, value: T) {
        // C++ CHECK(state == Ready): completing a promise twice is a crash.
        let func = self.func.take().expect("promise completed twice");
        func(Result::Ok(value));
    }

    fn set_error(&mut self, error: Status) {
        // C++ only fires while still Ready (moved-from/complete are no-ops).
        if let Some(func) = self.func.take() {
            func(Result::Err(error));
        }
    }
}

impl<T: Send> Drop for LambdaPromise<T> {
    fn drop(&mut self) {
        if let Some(func) = self.func.take() {
            func(Result::from_status(Status::error_message("Lost promise")));
        }
    }
}

/// The port of C++ `td::PromiseCreator`.
pub struct PromiseCreator;

impl PromiseCreator {
    /// C++ `PromiseCreator::lambda(f)` where `f` takes the value. The error
    /// path calls `f` with a default-constructed `T` — hence `T: Default`,
    /// mirroring the C++ `Auto()` conversion requirement.
    pub fn lambda<T, F>(f: F) -> Promise<T>
    where
        T: Send + Default + 'static,
        F: FnOnce(T) + Send + 'static,
    {
        Promise::from_interface(LambdaPromise::from_value_handler(f))
    }

    /// C++ `PromiseCreator::lambda(f)` where `f` takes a `Result<T>` (the
    /// result form, which C++ selects via `is_callable<F, Result<T>>`).
    pub fn lambda_result<T, F>(f: F) -> Promise<T>
    where
        T: Send + 'static,
        F: FnOnce(Result<T>) + Send + 'static,
    {
        Promise::from_interface(LambdaPromise::from_result_handler(f))
    }
}

/// The port of C++ `SafePromise<T>`: a promise plus its result; unless the
/// promise is [`release`](Self::release)d first, dropping the guard fulfills
/// the promise with the held result.
pub struct SafePromise<T: Send> {
    promise: Option<Promise<T>>,
    result: Result<T>,
}

impl<T: Send> SafePromise<T> {
    /// C++ `SafePromise(Promise<T> promise, Result<T> result)`.
    pub fn new(promise: Promise<T>, result: Result<T>) -> Self {
        Self {
            promise: Some(promise),
            result,
        }
    }

    /// C++ `release()`.
    pub fn release(&mut self) -> Promise<T> {
        self.promise.take().expect("SafePromise released twice")
    }
}

impl<T: Send> Drop for SafePromise<T> {
    fn drop(&mut self) {
        if let Some(mut promise) = self.promise.take() {
            // The C++ moved-from `result_` is unusable; Rust needs a live
            // placeholder, which `clear()`-style Error<-1> provides.
            let placeholder = Result::from_status(Status::error_code::<-1>());
            let result = std::mem::replace(&mut self.result, placeholder);
            promise.set_result(result);
        }
    }
}
