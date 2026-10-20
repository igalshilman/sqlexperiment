//! Bindings: the simplest extensions ([`crate::Extensions`]), an operation
//! and nothing else. A program calls one by name, `ctx.call(name, input)`;
//! the host makes it, outside Restate's journal. [`crate::Fetch`] is one.
//!
//! Nothing keeps a second copy of a call. Its input stays in the guest's
//! heap until it is answered, and the turn's commit keeps only the call's ID:
//! the host reads the input when it makes the call, and again on a retried
//! attempt. The result is delivered to the guest, and saved with its heap.
//!
//! A call is made after the commit that issued it, so its ID (and its
//! idempotency key, unless the guest set one) is the same on every attempt.
//! A crash after a call is made and before its result commits makes it
//! again, under the same key: a binding is called at least once. Only a
//! binding that [deduplicates](Binding::deduplicates) by its key takes a key
//! the guest sets. Calls last no longer than an attempt: a long one belongs
//! in a Restate service, called with `ctx.rpc.call`.
use crate::{
    GuestError,
    extension::{Attached, Extension, HostCall, Making, Tables},
};
use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;

/// An operation the host makes for a program.
pub trait Binding: Send + Sync + 'static {
    /// Make `call`: its result, or the error its promise rejects with.
    fn call(&self, call: HostCall) -> BoxFuture<'static, Result<Value, GuestError>>;

    /// Whether a call made again under the same idempotency key is answered
    /// as the first was, without being made again. Only then may the guest
    /// set a key of its own: on any other binding, a key would promise what
    /// the binding does not keep.
    fn deduplicates(&self) -> bool {
        false
    }
}

/// A binding is an extension: its operation, under any name.
#[async_trait]
impl<B: Binding + Clone> Extension for B {
    async fn attach(
        &self,
        _scope: &str,
        _saved: Option<Value>,
    ) -> anyhow::Result<Box<dyn Attached>> {
        Ok(Box::new(Bound(self.clone())))
    }
}

struct Bound<B>(B);

#[async_trait]
impl<B: Binding> Attached for Bound<B> {
    fn deduplicates(&self) -> bool {
        self.0.deduplicates()
    }

    async fn operation(
        &mut self,
        call: HostCall,
        _tables: &mut Tables<'_>,
    ) -> Result<Making, GuestError> {
        Ok(self.0.call(call))
    }
}
