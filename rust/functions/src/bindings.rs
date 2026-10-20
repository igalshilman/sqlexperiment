//! Host bindings: operations a program calls by name, `ctx.call(name,
//! input)`, that the host makes itself, outside Restate's journal. A
//! deployment offers them as [`Bindings`]; [`crate::Claude`] is one,
//! `claude.messages`.
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
use crate::GuestError;
use futures::future::BoxFuture;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

/// One call of a binding.
pub struct HostCall {
    /// The operation, `<run>/<n>`: the same on every attempt.
    pub id: String,
    /// The guest's idempotency key, if it set one, or else `id`.
    pub idempotency_key: String,
    /// The input, as the guest's heap holds it now.
    pub input: Value,
}

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

/// The bindings a deployment offers its programs, by name.
#[derive(Clone, Default)]
pub struct Bindings(BTreeMap<String, Arc<dyn Binding>>);

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer `binding` as `name`: `ctx.call(name, input)`.
    pub fn with(mut self, name: impl Into<String>, binding: impl Binding) -> Self {
        self.0.insert(name.into(), Arc::new(binding));
        self
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn Binding>> {
        self.0.get(name)
    }
}
