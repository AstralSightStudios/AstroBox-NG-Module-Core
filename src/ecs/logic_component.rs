use crate::ecs::component::{CompMeta, Component};
use crate::ecs::system::{HasSysMeta, System};

pub trait LogicComponent: Component {
    fn system(&self) -> &dyn System;
    fn system_mut(&mut self) -> &mut dyn System;
}

#[derive(serde::Serialize)]
pub struct LogicCompMeta {
    pub data: CompMeta,
    #[serde(skip_serializing)]
    pub system: Box<dyn System>,
}

impl LogicCompMeta {
    pub fn new<S>(id: &str) -> Self
    where
        S: System + HasSysMeta + Default + 'static,
    {
        let mut system: Box<S> = Box::default();
        system.meta_mut().id = id.to_string();
        Self {
            data: CompMeta {
                id: id.to_string(),
                owner: None,
            },
            system,
        }
    }
}

pub trait HasLogicCompMeta {
    fn meta(&self) -> &LogicCompMeta;
    fn meta_mut(&mut self) -> &mut LogicCompMeta;
}

#[macro_export]
macro_rules! impl_logic_component {
    ($ty:ty, $field:ident) => {
        impl $crate::ecs::logic_component::HasLogicCompMeta for $ty {
            fn meta(&self) -> &$crate::ecs::logic_component::LogicCompMeta {
                &self.$field
            }
            fn meta_mut(&mut self) -> &mut $crate::ecs::logic_component::LogicCompMeta {
                &mut self.$field
            }
        }

        impl $crate::ecs::component::HasCompMeta for $ty {
            fn meta(&self) -> &$crate::ecs::component::CompMeta {
                &self.$field.data
            }
            fn meta_mut(&mut self) -> &mut $crate::ecs::component::CompMeta {
                &mut self.$field.data
            }
        }

        impl $crate::ecs::component::Component for $ty {
            fn id(&self) -> &str {
                &self.$field.data.id
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
                self.$field.data.owner = Some(entity_id.to_string());
                self.$field.system.set_owner(entity_id);
            }
            fn owner(&self) -> Option<&str> {
                self.$field.data.owner.as_deref()
            }
            fn as_logic_component_mut(
                &mut self,
            ) -> Option<&mut dyn $crate::ecs::logic_component::LogicComponent> {
                Some(self)
            }
        }

        impl $crate::ecs::logic_component::LogicComponent for $ty {
            fn system(&self) -> &dyn $crate::ecs::system::System {
                self.$field.system.as_ref()
            }
            fn system_mut(&mut self) -> &mut dyn $crate::ecs::system::System {
                self.$field.system.as_mut()
            }
        }
    };
}
