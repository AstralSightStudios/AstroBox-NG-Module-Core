use std::any::Any;

use crate::ecs::logic_component::LogicComponent;

pub trait Component {
    fn id(&self) -> &str;

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
    fn set_owner(&mut self, _entity_id: &str) {}
    fn owner(&self) -> Option<&str> {
        None
    }

    fn as_logic_component_mut(&mut self) -> Option<&mut dyn LogicComponent> {
        None
    }
}

#[derive(Debug, Default, Clone)]
pub struct CompMeta {
    pub id: String,
    pub owner: Option<String>,
}

pub trait HasCompMeta {
    fn meta(&self) -> &CompMeta;
    fn meta_mut(&mut self) -> &mut CompMeta;
}

#[macro_export]
macro_rules! impl_has_comp_meta {
    ($ty:ty, $field:ident) => {
        impl $crate::ecs::component::HasCompMeta for $ty {
            fn meta(&self) -> &$crate::ecs::component::CompMeta {
                &self.$field
            }
            fn meta_mut(&mut self) -> &mut $crate::ecs::component::CompMeta {
                &mut self.$field
            }
        }
    };
}

#[macro_export]
macro_rules! impl_component {
    ($ty:ty, $field:ident) => {
        impl $crate::ecs::component::Component for $ty {
            fn id(&self) -> &str {
                &self.$field.id
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
            fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
                self
            }
            fn set_owner(&mut self, entity_id: &str) {
                self.$field.owner = Some(entity_id.to_string());
            }
            fn owner(&self) -> Option<&str> {
                self.$field.owner.as_deref()
            }
        }
    };
}
