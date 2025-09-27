use crate::ecs::{entity::Entity, system::System};
use std::collections::HashMap;

// ECS运行时环境，相当于World
// 存放了多个Entity和System，在AstroBox中一般一个设备一个Entity
pub struct Runtime {
    entities: HashMap<String, Box<dyn Entity>>,
    systems: HashMap<String, Box<dyn System>>,
}

impl Runtime {
    pub fn new() -> Runtime {
        Runtime {
            entities: HashMap::new(),
            systems: HashMap::new(),
        }
    }

    // 往Runtime中新增Entity
    pub fn add_entity<E: Entity + 'static>(&mut self, entity: E) {
        let id = entity.id().to_string();
        self.entities.insert(id, Box::new(entity));
    }

    // 在Runtime中根据ID寻找Entity并以指定类型返回（找不到返回None）
    pub fn find_entity_by_id_mut<T>(&mut self, id: &str) -> Option<&mut T>
    where
        T: Entity + 'static,
    {
        self.entities
            .get_mut(id)
            .and_then(|e| e.as_any_mut().downcast_mut::<T>())
    }

    // 在Runtime中根据ID寻找Entity并以dyn类型返回（这个给fastlane用的，一般用不上）
    pub fn find_entity_dyn_mut(&mut self, id: &str) -> Option<&mut dyn Entity> {
        self.entities.get_mut(id).map(|e| &mut **e)
    }

    // 在Runtime中根据ID卸载（删除）Entity并返回dyn类型
    pub fn remove_entity_by_id(&mut self, id: &str) -> Option<Box<dyn Entity>> {
        self.entities.remove(id)
    }

    // 往Runtime中新增System
    pub fn add_system<S: System + 'static>(&mut self, system: S) {
        let id = system.id().to_string();
        self.systems.insert(id, Box::new(system));
    }

    // 在Runtime中根据ID寻找System并以指定类型返回（找不到返回None）
    pub fn find_system_by_id_mut<T>(&mut self, id: &str) -> Option<&mut T>
    where
        T: System + 'static,
    {
        self.systems
            .get_mut(id)
            .and_then(|s| s.as_any_mut().downcast_mut::<T>())
    }

    // 在Runtime中根据ID卸载（删除）System并返回dyn类型
    pub fn remove_system_by_id(&mut self, id: &str) -> Option<Box<dyn System>> {
        self.systems.remove(id)
    }
}
