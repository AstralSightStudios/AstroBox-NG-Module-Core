use crate::ecs::component::Component;
use crate::ecs::entity::{Entity, LookupError};
use crate::ecs::logic_component::LogicComponent;
use crate::ecs::runtime::Runtime;
use crate::ecs::system::System;
use crate::ecs::with_rt_mut;

pub trait FastLane {
    fn with_entity_mut<R, F>(&self, f: F) -> Result<R, LookupError>
    where
        F: FnOnce(&mut dyn Entity) -> Result<R, LookupError> + Send + 'static,
        R: Send + 'static;

    fn with_component_mut<T, R, F>(&self, comp_id: &str, f: F) -> Result<R, LookupError>
    where
        T: Component + 'static,
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static;
}

impl FastLane for dyn LogicComponent {
    fn with_entity_mut<R, F>(&self, f: F) -> Result<R, LookupError>
    where
        F: FnOnce(&mut dyn Entity) -> Result<R, LookupError> + Send + 'static,
        R: Send + 'static,
    {
        let owner = self
            .owner()
            .ok_or_else(|| LookupError::NotFound {
                id: "<owner>".into(),
            })?
            .to_string();

        crate::asyncrt::universal_block_on(|| async move {
            with_rt_mut(move |rt: &mut Runtime| {
                let e = rt
                    .find_entity_dyn_mut(&owner)
                    .ok_or_else(|| LookupError::NotFound { id: owner.clone() })?;
                f(e)
            })
            .await
        })
    }

    fn with_component_mut<T, R, F>(&self, comp_id: &str, f: F) -> Result<R, LookupError>
    where
        T: Component + 'static,
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static,
    {
        let comp_id = comp_id.to_string();

        self.with_entity_mut(move |e| {
            let comp_any = e.get_component_mut(&comp_id)?;
            let comp_t = comp_any.as_any_mut().downcast_mut::<T>().ok_or_else(|| {
                LookupError::TypeMismatch {
                    id: comp_id.clone(),
                    expected: std::any::type_name::<T>(),
                    actual: std::any::type_name::<dyn Component>(),
                }
            })?;
            Ok::<R, LookupError>(f(comp_t))
        })
    }
}

impl FastLane for dyn System {
    fn with_entity_mut<R, F>(&self, f: F) -> Result<R, LookupError>
    where
        F: FnOnce(&mut dyn Entity) -> Result<R, LookupError> + Send + 'static,
        R: Send + 'static,
    {
        let owner = self
            .owner()
            .ok_or_else(|| LookupError::NotFound {
                id: "<owner>".into(),
            })?
            .to_string();

        crate::asyncrt::universal_block_on(|| async move {
            with_rt_mut(move |rt: &mut Runtime| {
                let e = rt
                    .find_entity_dyn_mut(&owner)
                    .ok_or_else(|| LookupError::NotFound { id: owner.clone() })?;
                f(e)
            })
            .await
        })
    }

    fn with_component_mut<T, R, F>(&self, comp_id: &str, f: F) -> Result<R, LookupError>
    where
        T: Component + 'static,
        F: FnOnce(&mut T) -> R + Send + 'static,
        R: Send + 'static,
    {
        let comp_id = comp_id.to_string();

        self.with_entity_mut(move |e| {
            let comp_any = e.get_component_mut(&comp_id)?;
            let comp_t = comp_any.as_any_mut().downcast_mut::<T>().ok_or_else(|| {
                LookupError::TypeMismatch {
                    id: comp_id.clone(),
                    expected: std::any::type_name::<T>(),
                    actual: std::any::type_name::<dyn Component>(),
                }
            })?;
            Ok::<R, LookupError>(f(comp_t))
        })
    }
}
