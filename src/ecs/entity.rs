use crate::ecs::component::Component;
use std::any::{Any, type_name};
use std::collections::HashMap;

// Lookup错误，实际上就是Find错误
// NotFound: 找不到目标id
// TypeMismatch: 目标ID的类型跟你期望的不匹配，id为...，期望的是...，实际上是...
#[derive(Debug)]
pub enum LookupError {
    NotFound {
        id: String,
    },
    TypeMismatch {
        id: String,
        expected: &'static str,
        actual: &'static str,
    },
}

pub trait Entity: Any {
    fn id(&self) -> &str;
    fn components(&mut self) -> &mut Vec<Box<dyn Component>>;

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;

    // 往Entity中新增Component
    fn add_component(&mut self, mut c: Box<dyn Component>) {
        c.set_owner(self.id());
        self.components().push(c);
    }

    // 在Entity上根据ID寻找Component并返回（dyn类型，非指定，指定属于ext impl）
    fn get_component_mut(&mut self, id: &str) -> Result<&mut dyn Component, LookupError> {
        let comps = self.components();
        let idx = comps
            .iter()
            .position(|c| c.id() == id)
            .ok_or_else(|| LookupError::NotFound { id: id.to_string() })?;
        Ok(&mut *comps[idx])
    }

    // 在Entity上根据ID删除Component并返回（dyn类型）
    fn remove_component_by_id(&mut self, id: &str) -> Result<Box<dyn Component>, LookupError> {
        let comps = self.components();
        let idx = comps
            .iter()
            .position(|c| c.id() == id)
            .ok_or_else(|| LookupError::NotFound { id: id.to_string() })?;
        Ok(comps.remove(idx))
    }
}

// 下面的as扩展和上面没啥大的区别，但允许自己指定类型
pub trait EntityExt: Entity {
    fn get_component_as_mut<T: Component + 'static>(
        &mut self,
        id: &str,
    ) -> Result<&mut T, LookupError> {
        let c = self.get_component_mut(id)?;
        c.as_any_mut()
            .downcast_mut::<T>()
            .ok_or_else(|| LookupError::TypeMismatch {
                id: id.to_string(),
                expected: type_name::<T>(),
                actual: std::any::type_name::<dyn Component>(),
            })
    }

    fn remove_component_as<T: Component + 'static>(&mut self, id: &str) -> Result<T, LookupError> {
        let b = self.remove_component_by_id(id)?;
        let any_box = b.into_any();
        any_box
            .downcast::<T>()
            .map(|bx| *bx)
            .map_err(|_| LookupError::TypeMismatch {
                id: id.to_string(),
                expected: type_name::<T>(),
                actual: "unknown-dyn-Component",
            })
    }
}

#[derive(Default)]
pub struct EntityMeta {
    pub id: String,
    pub components: Vec<Box<dyn Component>>,
    // 组件索引：id -> 下标 保持与components一致，便于O(1)索引
    // 用于针对ESP32等神笔低性能设备进行优化
    pub comp_index: HashMap<String, usize>,
}

pub trait HasEntityMeta {
    fn meta(&self) -> &EntityMeta;
    fn meta_mut(&mut self) -> &mut EntityMeta;
}

impl<T> Entity for T
where
    T: Any + HasEntityMeta + 'static,
{
    fn id(&self) -> &str {
        self.meta().id.as_str()
    }

    fn components(&mut self) -> &mut Vec<Box<dyn Component>> {
        &mut self.meta_mut().components
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    // 优化：维护 comp_index，使按ID查找O(1)
    fn add_component(&mut self, mut c: Box<dyn Component>) {
        c.set_owner(self.id());
        let id = c.id().to_string();
        let meta = self.meta_mut();
        meta.components.push(c);
        let idx = meta.components.len() - 1;
        meta.comp_index.insert(id, idx);
    }

    fn get_component_mut(&mut self, id: &str) -> Result<&mut dyn Component, LookupError> {
        // 先尝试通过索引表命中
        let mut need_rebuild = false;
        let idx_opt = {
            let meta = self.meta_mut();
            meta.comp_index.get(id).copied()
        };

        // 如果命中但指向的组件ID不匹配，则需要重建
        if let Some(i) = idx_opt {
            let mismatch = {
                let meta = self.meta_mut();
                meta.components.get(i).map(|c| c.id() != id).unwrap_or(true)
            };
            if mismatch {
                need_rebuild = true;
            }
        } else {
            // 未命中且组件不为空，可能索引未建立或被外部改动
            need_rebuild = {
                let meta = self.meta_mut();
                !meta.components.is_empty()
            };
        }

        if need_rebuild {
            let meta = self.meta_mut();
            meta.comp_index.clear();
            for (i, c) in meta.components.iter().enumerate() {
                meta.comp_index.insert(c.id().to_string(), i);
            }
        }

        let idx = {
            let meta = self.meta_mut();
            meta.comp_index
                .get(id)
                .copied()
                .or_else(|| meta.components.iter().position(|c| c.id() == id))
                .ok_or_else(|| LookupError::NotFound { id: id.to_string() })?
        };

        Ok(&mut *self.meta_mut().components[idx])
    }

    fn remove_component_by_id(&mut self, id: &str) -> Result<Box<dyn Component>, LookupError> {
        // 从索引表查找；未命中则重建一次
        let idx = {
            let mut idx = {
                let meta = self.meta_mut();
                meta.comp_index.get(id).copied()
            };
            if idx.is_none() {
                let meta = self.meta_mut();
                meta.comp_index.clear();
                for (i, c) in meta.components.iter().enumerate() {
                    meta.comp_index.insert(c.id().to_string(), i);
                }
                idx = meta.comp_index.get(id).copied();
            }
            idx.ok_or_else(|| LookupError::NotFound { id: id.to_string() })?
        };

        let meta = self.meta_mut();
        let removed = meta.components.remove(idx);
        // 维护索引表（后续元素下标左移）；先移除自身，再整体修正
        meta.comp_index.remove(id);
        for v in meta.comp_index.values_mut() {
            if *v > idx {
                *v -= 1;
            }
        }
        Ok(removed)
    }
}

#[macro_export]
macro_rules! impl_has_entity_meta {
    ($ty:ty, $field:ident) => {
        impl $crate::ecs::entity::HasEntityMeta for $ty {
            fn meta(&self) -> &$crate::ecs::entity::EntityMeta {
                &self.$field
            }
            fn meta_mut(&mut self) -> &mut $crate::ecs::entity::EntityMeta {
                &mut self.$field
            }
        }
    };
}

impl<E: Entity + ?Sized> EntityExt for E {}
