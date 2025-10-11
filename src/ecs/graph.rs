use crate::ecs::{component::Component, runtime::Runtime, with_rt_mut};
use serde::Serialize;
use serde_json::{Value, json};
use std::{any::type_name_of_val, collections::HashMap};

const RUNTIME_X: f64 = 0.0;
const RUNTIME_SYSTEM_X: f64 = 320.0;
const ENTITY_X: f64 = 480.0;
const COMPONENT_X: f64 = 960.0;
const LOGIC_SYSTEM_X: f64 = 1480.0;
const SYSTEM_X: f64 = 1980.0;
const ENTITY_SPACING_Y: f64 = 380.0;
const COMPONENT_SPACING_Y: f64 = 420.0;
const SYSTEM_SPACING_Y: f64 = 320.0;
const RUNTIME_SYSTEM_BASE_Y: f64 = -220.0;

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

#[derive(Clone)]
struct EntityLayout {
    node_id: String,
    position: ReactFlowPosition,
}

pub async fn export_react_flow_graph() -> ReactFlowGraph {
    with_rt_mut(|rt| build_graph(rt)).await
}

fn build_graph(rt: &mut Runtime) -> ReactFlowGraph {
    let mut nodes: Vec<ReactFlowNode> = Vec::new();
    let mut edges: Vec<ReactFlowEdge> = Vec::new();

    let mut systems_by_owner: HashMap<String, Vec<String>> = HashMap::new();
    for (system_id, system) in rt.systems.iter() {
        if let Some(owner) = system.owner() {
            systems_by_owner
                .entry(owner.to_string())
                .or_default()
                .push(system_id.clone());
        }
    }

    let runtime_node_id = "runtime".to_string();
    let runtime_position = ReactFlowPosition {
        x: RUNTIME_X,
        y: 0.0,
    };
    let runtime_node = ReactFlowNode {
        id: runtime_node_id.clone(),
        node_type: Some("input".to_string()),
        position: runtime_position,
        data: ReactFlowNodeData {
            label: "ECS Runtime".to_string(),
            kind: ReactFlowNodeKind::Runtime,
            type_name: "corelib::ecs::runtime::Runtime".to_string(),
            owner: None,
            extra: Some(json!({
                "entity_count": rt.entities.len(),
                "system_count": rt.systems.len(),
            })),
        },
    };
    nodes.push(runtime_node);

    let mut entity_ids: Vec<String> = rt.entities.keys().cloned().collect();
    entity_ids.sort();

    let mut entity_layout_map: HashMap<String, EntityLayout> = HashMap::new();

    for (entity_idx, entity_id) in entity_ids.iter().enumerate() {
        if let Some(entity) = rt.entities.get_mut(entity_id) {
            let entity_type = type_name_of_val(entity.as_any());
            let node_id = format!("entity:{entity_id}");
            let position = ReactFlowPosition {
                x: ENTITY_X,
                y: entity_idx as f64 * ENTITY_SPACING_Y,
            };

            let entity_details = snapshot_entity_details(entity.as_ref());

            let (component_ids, components_len) = {
                let comps = entity.components();
                let ids = comps.iter().map(|c| c.id().to_string()).collect::<Vec<_>>();
                (ids, comps.len())
            };
            let owned_systems = systems_by_owner.get(entity_id).cloned().unwrap_or_default();

            nodes.push(ReactFlowNode {
                id: node_id.clone(),
                node_type: None,
                position,
                data: ReactFlowNodeData {
                    label: format!("Entity: {entity_id}"),
                    kind: ReactFlowNodeKind::Entity,
                    type_name: entity_type.to_string(),
                    owner: None,
                    extra: Some(json!({
                        "component_count": components_len,
                        "components": component_ids,
                        "systems": owned_systems,
                        "data": entity_details,
                    })),
                },
            });

            edges.push(ReactFlowEdge {
                id: format!("edge:{}->{}", runtime_node_id, node_id),
                source: runtime_node_id.clone(),
                target: node_id.clone(),
                label: None,
            });

            entity_layout_map.insert(
                entity_id.clone(),
                EntityLayout {
                    node_id: node_id.clone(),
                    position,
                },
            );

            attach_components(
                entity_id,
                entity.as_mut(),
                &node_id,
                position,
                &mut nodes,
                &mut edges,
            );
        }
    }

    attach_runtime_systems(rt, &entity_layout_map, &mut nodes, &mut edges);

    ReactFlowGraph { nodes, edges }
}

fn attach_components(
    entity_id: &str,
    entity: &mut dyn crate::ecs::entity::Entity,
    entity_node_id: &str,
    entity_position: ReactFlowPosition,
    nodes: &mut Vec<ReactFlowNode>,
    edges: &mut Vec<ReactFlowEdge>,
) {
    let components = entity.components();
    for (comp_idx, component) in components.iter_mut().enumerate() {
        let component_type = type_name_of_val(component.as_any());
        let component_id = component.id().to_string();
        let component_label = format!("Component: {}", component_id);
        let node_id = format!("component:{entity_id}:{component_id}");
        let position = ReactFlowPosition {
            x: COMPONENT_X,
            y: entity_position.y + COMPONENT_SPACING_Y * comp_idx as f64,
        };
        let owner = component.owner().map(|s| s.to_string());

        nodes.push(ReactFlowNode {
            id: node_id.clone(),
            node_type: None,
            position,
            data: ReactFlowNodeData {
                label: component_label,
                kind: ReactFlowNodeKind::Component,
                type_name: component_type.to_string(),
                owner: owner.clone(),
                extra: Some(json!({
                    "order": comp_idx + 1,
                    "owner": owner,
                    "data": snapshot_component_details(&**component),
                })),
            },
        });

        edges.push(ReactFlowEdge {
            id: format!("edge:{}->{}", entity_node_id, node_id),
            source: entity_node_id.to_string(),
            target: node_id.clone(),
            label: None,
        });

        if let Some(logic_component) = component.as_logic_component_mut() {
            let system = logic_component.system();
            let system_type = type_name_of_val(system.as_any());
            let system_id = system.id().to_string();
            let system_node_id = format!("logic-system:{entity_id}:{system_id}");
            let system_owner = system.owner().map(|s| s.to_string());
            nodes.push(ReactFlowNode {
                id: system_node_id.clone(),
                node_type: None,
                position: ReactFlowPosition {
                    x: LOGIC_SYSTEM_X,
                    y: position.y,
                },
                data: ReactFlowNodeData {
                    label: format!("LogicSystem: {}", system.id()),
                    kind: ReactFlowNodeKind::LogicSystem,
                    type_name: system_type.to_string(),
                    owner: system_owner.clone(),
                    extra: Some(json!({
                        "component": component_id,
                        "owner": system_owner,
                        "data": snapshot_component_details(&**component),
                    })),
                },
            });

            edges.push(ReactFlowEdge {
                id: format!("edge:{}->{}", node_id, system_node_id),
                source: node_id,
                target: system_node_id,
                label: None,
            });
        }
    }
}

fn attach_runtime_systems(
    rt: &mut Runtime,
    entity_layout_map: &HashMap<String, EntityLayout>,
    nodes: &mut Vec<ReactFlowNode>,
    edges: &mut Vec<ReactFlowEdge>,
) {
    let mut system_ids: Vec<String> = rt.systems.keys().cloned().collect();
    system_ids.sort();

    let mut owner_offsets: HashMap<Option<String>, usize> = HashMap::new();

    for system_id in system_ids.iter() {
        if let Some(system) = rt.systems.get_mut(system_id) {
            let system_type = type_name_of_val(system.as_any());
            let node_id = format!("system:{system_id}");
            let owner = system.owner().map(|s| s.to_string());

            let offset_idx = owner_offsets.entry(owner.clone()).or_insert(0usize);
            let local_idx = *offset_idx;
            *offset_idx += 1;

            let (target_owner_node, position) = if let Some(ref owner_id) = owner {
                if let Some(layout) = entity_layout_map.get(owner_id) {
                    (
                        layout.node_id.clone(),
                        ReactFlowPosition {
                            x: SYSTEM_X,
                            y: layout.position.y + SYSTEM_SPACING_Y * local_idx as f64,
                        },
                    )
                } else {
                    (
                        "runtime".to_string(),
                        ReactFlowPosition {
                            x: RUNTIME_SYSTEM_X,
                            y: SYSTEM_SPACING_Y * local_idx as f64,
                        },
                    )
                }
            } else {
                (
                    "runtime".to_string(),
                    ReactFlowPosition {
                        x: RUNTIME_SYSTEM_X,
                        y: RUNTIME_SYSTEM_BASE_Y + SYSTEM_SPACING_Y * local_idx as f64,
                    },
                )
            };

            nodes.push(ReactFlowNode {
                id: node_id.clone(),
                node_type: None,
                position,
                data: ReactFlowNodeData {
                    label: format!("System: {system_id}"),
                    kind: ReactFlowNodeKind::System,
                    type_name: system_type.to_string(),
                    owner: owner.clone(),
                    extra: None,
                },
            });

            edges.push(ReactFlowEdge {
                id: format!("edge:{}->{}", target_owner_node, node_id),
                source: target_owner_node,
                target: node_id,
                label: None,
            });
        }
    }
}

fn snapshot_entity_details(entity: &dyn crate::ecs::entity::Entity) -> Value {
    serde_json::to_value(entity).unwrap_or(Value::Null)
}

fn snapshot_component_details(component: &dyn Component) -> Value {
    serde_json::to_value(component).unwrap_or_else(|_| {
        json!({
            "type": type_name_of_val(component.as_any()),
        })
    })
}
