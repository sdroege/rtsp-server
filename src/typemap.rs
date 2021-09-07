// Rust RTSP Server
//
// Copyright (C) 2020-2021 Sebastian Dröge <sebastian@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct TypeMap {
    map: Option<Arc<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
}

impl Default for TypeMap {
    fn default() -> Self {
        TypeMap { map: None }
    }
}

impl TypeMap {
    pub fn get<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.map
            .as_ref()
            .and_then(|map| map.get(&TypeId::of::<T>()))
            .and_then(|v| v.downcast_ref())
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map
            .as_ref()
            .map(|map| map.contains_key(&TypeId::of::<T>()))
            .unwrap_or(false)
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, v: T) {
        if self.map.is_none() {
            self.map = Some(Arc::new(HashMap::new()));
        }

        let map = self.map.as_mut().expect("no map");

        Arc::make_mut(map).insert(TypeId::of::<T>(), Arc::new(v));
    }

    pub fn remove<T: Send + Sync + 'static>(&mut self) {
        if let Some(ref mut map) = self.map {
            Arc::make_mut(map).remove(&TypeId::of::<T>());
        }
    }

    pub fn clear(&mut self) {
        self.map = None;
    }

    pub fn extend(&mut self, other: &Self) {
        let other_map = match other.map {
            None => return,
            Some(ref other) => other,
        };

        if self.map.is_none() {
            self.map = Some(Arc::new(HashMap::new()));
        }

        let map = self.map.as_mut().expect("no map");

        Arc::make_mut(map).extend(other_map.iter().map(|(tid, item)| (*tid, item.clone())));
    }
}
