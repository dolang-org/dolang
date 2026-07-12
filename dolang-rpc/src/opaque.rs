use serde::{Deserialize, Serialize};
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    marker::PhantomData,
    sync::Arc,
};

/// A value which can be stored in a session's opaque-object table.
pub trait OpaqueResource: Send + Sync + 'static {
    type Marker: ?Sized + 'static;
}

#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
pub struct Opaque<M: ?Sized> {
    owner: u8,
    id: u64,
    marker: PhantomData<fn() -> M>,
}
impl<M: ?Sized> Copy for Opaque<M> {}
impl<M: ?Sized> Clone for Opaque<M> {
    fn clone(&self) -> Self {
        *self
    }
}

/// A retained, typed opaque object.
pub struct OpaqueGuard<T>(Arc<T>);
impl<T> std::ops::Deref for OpaqueGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error)]
#[error("invalid opaque object")]
pub struct InvalidOpaque;

#[derive(Default)]
pub(crate) struct ObjectTable {
    next: u64,
    values: HashMap<u64, (TypeId, Arc<dyn Any + Send + Sync>)>,
}

impl ObjectTable {
    pub fn register<T: OpaqueResource>(&mut self, value: T) -> Opaque<T::Marker> {
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .expect("opaque identifiers exhausted");
        self.values.insert(id, (TypeId::of::<T>(), Arc::new(value)));
        Opaque {
            owner: 1,
            id,
            marker: PhantomData,
        }
    }
    pub fn acquire<T: OpaqueResource>(
        &self,
        value: Opaque<T::Marker>,
    ) -> Result<OpaqueGuard<T>, InvalidOpaque> {
        if value.owner != 1 {
            return Err(InvalidOpaque);
        }
        let (ty, erased) = self.values.get(&value.id).ok_or(InvalidOpaque)?;
        if *ty != TypeId::of::<T>() {
            return Err(InvalidOpaque);
        }
        Ok(OpaqueGuard(
            erased.clone().downcast::<T>().map_err(|_| InvalidOpaque)?,
        ))
    }
    pub fn unregister<M: ?Sized + 'static>(
        &mut self,
        value: Opaque<M>,
    ) -> Result<(), InvalidOpaque> {
        self.values
            .remove(&value.id)
            .map(|_| ())
            .ok_or(InvalidOpaque)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Marker;
    struct Value(u32);
    impl OpaqueResource for Value {
        type Marker = Marker;
    }

    #[test]
    fn guards_outlive_registration() {
        let mut table = ObjectTable::default();
        let opaque = table.register(Value(42));
        let guard = table.acquire::<Value>(opaque).unwrap();
        table.unregister(opaque).unwrap();
        assert_eq!(guard.0.0, 42);
        assert!(table.acquire::<Value>(opaque).is_err());
    }
}
