use serde::{Deserialize, Serialize};
use std::{
    any::{Any, TypeId},
    collections::HashMap,
    fmt,
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

impl<M: ?Sized> fmt::Debug for Opaque<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opaque")
            .field("owner", &self.owner)
            .field("id", &self.id)
            .finish_non_exhaustive()
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
    pub fn unregister<T: OpaqueResource>(
        &mut self,
        value: Opaque<T::Marker>,
    ) -> Result<Option<T>, InvalidOpaque> {
        if value.owner != 1 {
            return Err(InvalidOpaque);
        }
        let (ty, _) = self.values.get(&value.id).ok_or(InvalidOpaque)?;
        if *ty != TypeId::of::<T>() {
            return Err(InvalidOpaque);
        }
        let (_, erased) = self.values.remove(&value.id).unwrap();
        let value = erased.downcast::<T>().map_err(|_| InvalidOpaque)?;
        Ok(Arc::try_unwrap(value).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Marker;
    struct OtherMarker;
    struct Value(u32);
    struct OtherValue;
    struct DropValue(Arc<AtomicBool>);
    impl OpaqueResource for Value {
        type Marker = Marker;
    }
    impl OpaqueResource for OtherValue {
        type Marker = OtherMarker;
    }
    impl OpaqueResource for DropValue {
        type Marker = Marker;
    }
    impl Drop for DropValue {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn guards_outlive_registration() {
        let mut table = ObjectTable::default();
        let opaque = table.register(Value(42));
        let guard = table.acquire::<Value>(opaque).unwrap();
        assert!(table.unregister::<Value>(opaque).unwrap().is_none());
        assert_eq!(guard.0.0, 42);
        assert!(table.acquire::<Value>(opaque).is_err());
    }

    #[test]
    fn unregister_returns_exclusively_owned_value() {
        let mut table = ObjectTable::default();
        let opaque = table.register(Value(42));
        let value = table.unregister::<Value>(opaque).unwrap().unwrap();
        assert_eq!(value.0, 42);
    }

    #[test]
    fn wrong_type_does_not_remove_value() {
        let mut table = ObjectTable::default();
        let opaque = table.register(Value(42));
        let wrong = Opaque::<OtherMarker> {
            owner: opaque.owner,
            id: opaque.id,
            marker: PhantomData,
        };
        assert!(table.unregister::<OtherValue>(wrong).is_err());
        assert_eq!(table.acquire::<Value>(opaque).unwrap().0.0, 42);
    }

    #[test]
    fn dropping_table_drops_registered_values() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut table = ObjectTable::default();
        table.register(DropValue(dropped.clone()));
        drop(table);
        assert!(dropped.load(Ordering::Relaxed));
    }
}
