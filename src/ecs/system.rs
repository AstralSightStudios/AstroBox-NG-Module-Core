use std::any::Any;

pub trait System {
    fn id(&self) -> &str;

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn into_any(self: Box<Self>) -> Box<dyn Any>;

    fn set_owner(&mut self, _entity_id: &str) {}
    fn owner(&self) -> Option<&str> {
        None
    }
}

#[derive(Debug, Default, Clone)]
pub struct SysMeta {
    pub id: String,
    pub owner: Option<String>,
}

pub trait HasSysMeta {
    fn meta(&self) -> &SysMeta;
    fn meta_mut(&mut self) -> &mut SysMeta;
}

impl<T> System for T
where
    T: Any + HasSysMeta + 'static,
{
    fn id(&self) -> &str {
        &self.meta().id
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }

    fn set_owner(&mut self, entity_id: &str) {
        self.meta_mut().owner = Some(entity_id.to_string());
    }
    fn owner(&self) -> Option<&str> {
        self.meta().owner.as_deref()
    }
}

#[macro_export]
macro_rules! impl_has_sys_meta {
    ($ty:ty, $field:ident) => {
        impl $crate::ecs::system::HasSysMeta for $ty {
            fn meta(&self) -> &$crate::ecs::system::SysMeta {
                &self.$field
            }
            fn meta_mut(&mut self) -> &mut $crate::ecs::system::SysMeta {
                &mut self.$field
            }
        }
    };
}
