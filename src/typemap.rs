use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TypeMap {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Default for TypeMap {
    fn default() -> Self {
        TypeMap {
            map: HashMap::new(),
        }
    }
}

impl TypeMap {
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .get(&TypeId::of::<T>())
            .and_then(|v| v.downcast_ref())
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, v: T) {
        self.map.insert(TypeId::of::<T>(), Arc::new(v));
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) {
        self.map.remove(&TypeId::of::<T>());
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    pub fn extend(&mut self, other: &Self) {
        self.map
            .extend(other.map.iter().map(|(tid, item)| (*tid, item.clone())));
    }
}
