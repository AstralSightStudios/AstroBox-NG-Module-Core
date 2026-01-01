use crate::{
    device::xiaomi::{
        XiaomiDevice,
        components::{
            auth::{AuthComponent, AuthSystem},
            info::{InfoComponent, InfoSystem},
            install::{InstallComponent, InstallSystem},
            mass::{MassComponent, MassSystem},
            resource::{ResourceComponent, ResourceSystem},
            sync::{SyncComponent, SyncSystem},
            thirdparty_app::{ThirdpartyAppComponent, ThirdpartyAppSystem},
            watchface::{WatchfaceComponent, WatchfaceSystem},
        },
    },
    ecs::runtime::Runtime,
    ecs::with_rt_mut,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::device::xiaomi::components::network::{NetworkComponent, NetworkSystem};
use bevy_ecs::{component::Component, entity::Entity, world::World};
use serde::Serialize;
use serde_json::{Value, json};
use std::{any::type_name, collections::HashMap};

const RUNTIME_X: f64 = 0.0;
const ENTITY_X: f64 = 480.0;
const COMPONENT_X: f64 = 960.0;
const LOGIC_SYSTEM_X: f64 = 1480.0;
const ENTITY_SPACING_Y: f64 = 380.0;
const COMPONENT_SPACING_Y: f64 = 420.0;
const SYSTEM_SPACING_Y: f64 = 320.0;

#[derive(Debug, Serialize)]
pub struct ReactFlowGraph {
    pub nodes: Vec<ReactFlowNode>,
    pub edges: Vec<ReactFlowEdge>,
}

#[derive(Debug, Serialize)]
pub struct ReactFlowNode {
    pub id: String,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub node_type: Option<String>,
    pub position: ReactFlowPosition,
    pub data: ReactFlowNodeData,
}

#[derive(Debug, Serialize, Clone, Copy)]
pub struct ReactFlowPosition {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactFlowNodeKind {
    Runtime,
    Entity,
    Component,
    System,
    LogicSystem,
}

#[derive(Debug, Serialize)]
pub struct ReactFlowNodeData {
    pub label: String,
    pub kind: ReactFlowNodeKind,
    pub type_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ReactFlowEdge {
    pub id: String,
    pub source: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

pub async fn export_react_flow_graph() -> ReactFlowGraph {
    with_rt_mut(|rt| build_graph(rt)).await
}

fn build_graph(rt: &mut Runtime) -> ReactFlowGraph {
    let world = rt.world();
    let mut nodes: Vec<ReactFlowNode> = Vec::new();
    let mut edges: Vec<ReactFlowEdge> = Vec::new();

    let runtime_node_id = "runtime".to_string();
    nodes.push(ReactFlowNode {
        id: runtime_node_id.clone(),
        node_type: Some("input".to_string()),
        position: ReactFlowPosition { x: RUNTIME_X, y: 0.0 },
        data: ReactFlowNodeData {
            label: "ECS Runtime".to_string(),
            kind: ReactFlowNodeKind::Runtime,
            type_name: "corelib::ecs::runtime::Runtime".to_string(),
            owner: None,
            extra: Some(json!({
                "entity_count": rt.device_count(),
            })),
        },
    });

    let mut device_ids: Vec<String> = rt.device_ids().cloned().collect();
    device_ids.sort();

    for (entity_idx, device_id) in device_ids.iter().enumerate() {
        let entity = match rt.device_entity(device_id) {
            Some(entity) => entity,
            None => continue,
        };

        let node_id = format!("entity:{device_id}");
        let position = ReactFlowPosition {
            x: ENTITY_X,
            y: entity_idx as f64 * ENTITY_SPACING_Y,
        };
        let entity_details = world
            .get::<XiaomiDevice>(entity)
            .and_then(|dev| serde_json::to_value(dev).ok())
            .unwrap_or(Value::Null);

        let mut component_nodes: HashMap<String, String> = HashMap::new();
        let mut component_labels: Vec<String> = Vec::new();
        let mut system_labels: Vec<String> = Vec::new();
        let mut comp_idx = 0usize;

        add_component_node::<AuthComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        add_component_node::<InstallComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        add_component_node::<MassComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        add_component_node::<InfoComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        add_component_node::<ThirdpartyAppComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        add_component_node::<ResourceComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        add_component_node::<WatchfaceComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        add_component_node::<SyncComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );
        #[cfg(not(target_arch = "wasm32"))]
        add_component_node::<NetworkComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &mut comp_idx,
            &mut component_labels,
            &mut component_nodes,
            &mut nodes,
            &mut edges,
        );

        add_system_node::<AuthSystem, AuthComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        add_system_node::<InstallSystem, InstallComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        add_system_node::<MassSystem, MassComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        add_system_node::<InfoSystem, InfoComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        add_system_node::<ThirdpartyAppSystem, ThirdpartyAppComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        add_system_node::<ResourceSystem, ResourceComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        add_system_node::<WatchfaceSystem, WatchfaceComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        add_system_node::<SyncSystem, SyncComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );
        #[cfg(not(target_arch = "wasm32"))]
        add_system_node::<NetworkSystem, NetworkComponent>(
            world,
            entity,
            device_id,
            &node_id,
            position,
            &component_nodes,
            &mut system_labels,
            &mut nodes,
            &mut edges,
        );

        nodes.push(ReactFlowNode {
            id: node_id.clone(),
            node_type: None,
            position,
            data: ReactFlowNodeData {
                label: format!("Entity: {device_id}"),
                kind: ReactFlowNodeKind::Entity,
                type_name: type_name::<XiaomiDevice>().to_string(),
                owner: None,
                extra: Some(json!({
                    "component_count": component_labels.len(),
                    "components": component_labels,
                    "systems": system_labels,
                    "data": entity_details,
                })),
            },
        });

        edges.push(ReactFlowEdge {
            id: format!("edge:{}->{}", runtime_node_id, node_id),
            source: runtime_node_id.clone(),
            target: node_id,
            label: None,
        });
    }

    ReactFlowGraph { nodes, edges }
}

fn add_component_node<T: Component + Serialize>(
    world: &World,
    entity: Entity,
    device_id: &str,
    entity_node_id: &str,
    entity_position: ReactFlowPosition,
    comp_idx: &mut usize,
    labels: &mut Vec<String>,
    node_ids: &mut HashMap<String, String>,
    nodes: &mut Vec<ReactFlowNode>,
    edges: &mut Vec<ReactFlowEdge>,
) {
    let comp = match world.get::<T>(entity) {
        Some(comp) => comp,
        None => return,
    };
    let component_type = type_name::<T>();
    let node_id = format!("component:{device_id}:{component_type}");
    let position = ReactFlowPosition {
        x: COMPONENT_X,
        y: entity_position.y + COMPONENT_SPACING_Y * *comp_idx as f64,
    };
    let data = serde_json::to_value(comp).unwrap_or(Value::Null);

    nodes.push(ReactFlowNode {
        id: node_id.clone(),
        node_type: None,
        position,
        data: ReactFlowNodeData {
            label: format!("Component: {component_type}"),
            kind: ReactFlowNodeKind::Component,
            type_name: component_type.to_string(),
            owner: Some(device_id.to_string()),
            extra: Some(json!({
                "order": *comp_idx + 1,
                "owner": device_id,
                "data": data,
            })),
        },
    });

    edges.push(ReactFlowEdge {
        id: format!("edge:{}->{}", entity_node_id, node_id),
        source: entity_node_id.to_string(),
        target: node_id.clone(),
        label: None,
    });

    labels.push(component_type.to_string());
    node_ids.insert(component_type.to_string(), node_id);
    *comp_idx += 1;
}

fn add_system_node<S: Component, C: Component>(
    world: &World,
    entity: Entity,
    device_id: &str,
    entity_node_id: &str,
    entity_position: ReactFlowPosition,
    component_nodes: &HashMap<String, String>,
    labels: &mut Vec<String>,
    nodes: &mut Vec<ReactFlowNode>,
    edges: &mut Vec<ReactFlowEdge>,
) {
    if world.get::<S>(entity).is_none() {
        return;
    }
    let system_type = type_name::<S>();
    let component_type = type_name::<C>();
    let source_node_id = component_nodes
        .get(component_type)
        .cloned()
        .unwrap_or_else(|| entity_node_id.to_string());
    let local_idx = labels.len();

    let node_id = format!("logic-system:{device_id}:{system_type}");
    let position = ReactFlowPosition {
        x: LOGIC_SYSTEM_X,
        y: entity_position.y + SYSTEM_SPACING_Y * local_idx as f64,
    };

    nodes.push(ReactFlowNode {
        id: node_id.clone(),
        node_type: None,
        position,
        data: ReactFlowNodeData {
            label: format!("LogicSystem: {system_type}"),
            kind: ReactFlowNodeKind::LogicSystem,
            type_name: system_type.to_string(),
            owner: Some(device_id.to_string()),
            extra: None,
        },
    });

    edges.push(ReactFlowEdge {
        id: format!("edge:{}->{}", source_node_id, node_id),
        source: source_node_id,
        target: node_id,
        label: None,
    });

    labels.push(system_type.to_string());
}
