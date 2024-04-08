// Copyright (c) 2024 - Restate Software, Inc., Restate GmbH.
// All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use crate::schema_registry::error::{
    ComponentError, DeploymentError, SchemaError, SubscriptionError,
};
use crate::schema_registry::ComponentName;
use http::{HeaderValue, Uri};
use restate_schema::component::{ComponentLocation, ComponentSchemas, HandlerSchemas};
use restate_schema::deployment::DeploymentSchemas;
use restate_schema::SchemaInformation;
use restate_schema_api::component::{ComponentType, HandlerType};
use restate_schema_api::deployment::DeploymentMetadata;
use restate_schema_api::invocation_target::{
    InputRules, InputValidationRule, InvocationTargetMetadata, OutputContentTypeRule, OutputRules,
};
use restate_schema_api::subscription::{
    EventReceiverComponentType, Sink, Source, Subscription, SubscriptionValidator,
};
use restate_service_protocol::discovery::schema;
use restate_types::identifiers::{DeploymentId, SubscriptionId};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use tracing::{info, warn};

/// Responsible for updating the provided [`SchemaInformation`] with new
/// schema information. It makes sure that the version of schema information
/// is incremented on changes.
#[derive(Debug, Default)]
pub struct SchemaUpdater {
    schema_information: SchemaInformation,
    modified: bool,
}

impl From<SchemaInformation> for SchemaUpdater {
    fn from(schema_information: SchemaInformation) -> Self {
        Self {
            schema_information,
            modified: false,
        }
    }
}

impl SchemaUpdater {
    pub fn into_inner(mut self) -> SchemaInformation {
        if self.modified {
            self.schema_information.increment_version()
        }

        self.schema_information
    }

    pub fn add_deployment(
        &mut self,
        requested_deployment_id: Option<DeploymentId>,
        deployment_metadata: DeploymentMetadata,
        components: Vec<schema::Component>,
        force: bool,
    ) -> Result<DeploymentId, SchemaError> {
        let deployment_id: Option<DeploymentId>;

        let proposed_components: HashMap<_, _> = components
            .into_iter()
            .map(|c| {
                ComponentName::try_from(c.fully_qualified_component_name.to_string())
                    .map(|name| (name, c))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        // Did we find an existing deployment with same id or with a conflicting endpoint url?
        let found_existing_deployment = requested_deployment_id
            .and_then(|id| self.schema_information.find_existing_deployment_by_id(&id))
            .or_else(|| {
                self.schema_information
                    .find_existing_deployment_by_endpoint(&deployment_metadata.ty)
            });

        let mut components_to_remove = Vec::default();

        if let Some((existing_deployment_id, existing_deployment)) = found_existing_deployment {
            if requested_deployment_id.is_some_and(|dp| &dp != existing_deployment_id) {
                // The deployment id is different from the existing one, we don't accept that even
                // if force is used. It means that the user intended to update another deployment.
                return Err(SchemaError::Deployment(DeploymentError::IncorrectId {
                    requested: requested_deployment_id.expect("must be set"),
                    existing: *existing_deployment_id,
                }));
            }

            if force {
                deployment_id = Some(*existing_deployment_id);

                for component in &existing_deployment.components {
                    // If a component is not available anymore in the new deployment, we need to remove it
                    if !proposed_components.contains_key(&component.name) {
                        warn!(
                            restate.deployment.id = %existing_deployment_id,
                            restate.deployment.address = %deployment_metadata.address_display(),
                            "Going to remove component {} due to a forced deployment update",
                            component.name
                        );
                        components_to_remove.push(component.name.clone());
                    }
                }
            } else {
                return Err(SchemaError::Override(format!(
                    "deployment with id '{existing_deployment_id}'"
                )));
            }
        } else {
            // New deployment. Use the supplied deployment_id if passed, otherwise, generate one.
            deployment_id = requested_deployment_id.or_else(|| Some(DeploymentId::new()));
        }

        // We must have a deployment id by now, either a new or existing one.
        let deployment_id = deployment_id.unwrap();

        let mut components_to_add = HashMap::with_capacity(proposed_components.len());

        // Compute component schemas
        for (component_name, component) in proposed_components {
            let component_type = ComponentType::from(component.component_type);
            let handlers = DiscoveredHandlerMetadata::compute_handlers(
                component_type,
                component
                    .handlers
                    .into_iter()
                    .map(|h| DiscoveredHandlerMetadata::from_schema(component_type, h))
                    .collect::<Result<Vec<_>, _>>()?,
            );

            // For the time being when updating we overwrite existing data
            let component_schema = if let Some(existing_component) = self
                .schema_information
                .components
                .get(component_name.as_ref())
            {
                let removed_handlers: Vec<String> = existing_component
                    .handlers
                    .keys()
                    .filter(|name| !handlers.contains_key(*name))
                    .map(|name| name.to_string())
                    .collect();

                if !removed_handlers.is_empty() {
                    if force {
                        warn!(
                            restate.deployment.id = %deployment_id,
                            restate.deployment.address = %deployment_metadata.address_display(),
                            "Going to remove the following methods from component type {} due to a forced deployment update: {:?}.",
                            component.fully_qualified_component_name.as_str(),
                            removed_handlers
                        );
                    } else {
                        return Err(SchemaError::Component(ComponentError::RemovedHandlers(
                            component_name,
                            removed_handlers,
                        )));
                    }
                }

                if existing_component.ty != component_type {
                    if force {
                        warn!(
                            restate.deployment.id = %deployment_id,
                            restate.deployment.address = %deployment_metadata.address_display(),
                            "Going to overwrite component type {} due to a forced deployment update: {:?} != {:?}. This is a potentially dangerous operation, and might result in data loss.",
                            component_name,
                            existing_component.ty,
                            component_type
                        );
                    } else {
                        return Err(SchemaError::Component(ComponentError::DifferentType(
                            component_name,
                        )));
                    }
                }

                info!(
                    rpc.service = %component_name,
                    "Overwriting existing component schemas"
                );
                let mut component_schemas = existing_component.clone();
                component_schemas.revision = existing_component.revision.wrapping_add(1);
                component_schemas.ty = component_type;
                component_schemas.handlers = handlers;
                component_schemas.location.latest_deployment = deployment_id;

                component_schemas
            } else {
                ComponentSchemas {
                    revision: 1,
                    handlers,
                    ty: component_type,
                    location: ComponentLocation {
                        latest_deployment: deployment_id,
                        public: true,
                    },
                }
            };

            components_to_add.insert(component_name, component_schema);
        }

        for component_to_remove in components_to_remove {
            self.schema_information
                .components
                .remove(&component_to_remove);
        }

        let components_metadata = components_to_add
            .into_iter()
            .map(|(name, schema)| {
                let metadata = schema.as_component_metadata(name.clone().into_inner());
                self.schema_information
                    .components
                    .insert(name.into_inner(), schema);
                metadata
            })
            .collect();

        self.schema_information.deployments.insert(
            deployment_id,
            DeploymentSchemas {
                components: components_metadata,
                metadata: deployment_metadata,
            },
        );

        self.modified = true;

        Ok(deployment_id)
    }

    pub fn remove_deployment(&mut self, deployment_id: DeploymentId) {
        if let Some(deployment) = self.schema_information.deployments.remove(&deployment_id) {
            for component_metadata in deployment.components {
                match self
                    .schema_information
                    .components
                    .entry(component_metadata.name)
                {
                    // we need to check for the right revision in the component has been overwritten
                    // by a different deployment.
                    Entry::Occupied(entry)
                        if entry.get().revision == component_metadata.revision =>
                    {
                        entry.remove();
                    }
                    _ => {}
                }
            }
            self.modified = true;
        }
    }

    pub fn add_subscription<V: SubscriptionValidator>(
        &mut self,
        id: Option<SubscriptionId>,
        source: Uri,
        sink: Uri,
        metadata: Option<HashMap<String, String>>,
        validator: &V,
    ) -> Result<SubscriptionId, SchemaError> {
        // generate id if not provided
        let id = id.unwrap_or_default();

        if self.schema_information.subscriptions.contains_key(&id) {
            return Err(SchemaError::Override(format!(
                "subscription with id '{id}'"
            )));
        }

        // TODO This logic to parse source and sink should be moved elsewhere to abstract over the known source/sink providers
        //  Maybe together with the validator?

        // Parse source
        let source = match source.scheme_str() {
            Some("kafka") => {
                let cluster_name = source
                    .authority()
                    .ok_or_else(|| {
                        SchemaError::Subscription(SubscriptionError::InvalidKafkaSourceAuthority(
                            source.clone(),
                        ))
                    })?
                    .as_str();
                let topic_name = &source.path()[1..];
                Source::Kafka {
                    cluster: cluster_name.to_string(),
                    topic: topic_name.to_string(),
                    ordering_key_format: Default::default(),
                }
            }
            _ => {
                return Err(SchemaError::Subscription(
                    SubscriptionError::InvalidSourceScheme(source),
                ))
            }
        };

        // Parse sink
        let sink = match sink.scheme_str() {
            Some("component") => {
                let component_name = sink
                    .authority()
                    .ok_or_else(|| {
                        SchemaError::Subscription(SubscriptionError::InvalidComponentSinkAuthority(
                            sink.clone(),
                        ))
                    })?
                    .as_str();
                let handler_name = &sink.path()[1..];

                // Retrieve component and handler in the schema registry
                let component_schemas = self
                    .schema_information
                    .components
                    .get(component_name)
                    .ok_or_else(|| {
                        SchemaError::Subscription(SubscriptionError::SinkComponentNotFound(
                            sink.clone(),
                        ))
                    })?;
                if !component_schemas.handlers.contains_key(handler_name) {
                    return Err(SchemaError::Subscription(
                        SubscriptionError::SinkComponentNotFound(sink),
                    ));
                }

                let ty = match component_schemas.ty {
                    ComponentType::VirtualObject => EventReceiverComponentType::VirtualObject {
                        ordering_key_is_key: false,
                    },
                    ComponentType::Service => EventReceiverComponentType::Service,
                };

                Sink::Component {
                    name: component_name.to_owned(),
                    handler: handler_name.to_owned(),
                    ty,
                }
            }
            _ => {
                return Err(SchemaError::Subscription(
                    SubscriptionError::InvalidSinkScheme(sink),
                ))
            }
        };

        let subscription = validator
            .validate(Subscription::new(
                id,
                source,
                sink,
                metadata.unwrap_or_default(),
            ))
            .map_err(|e| SchemaError::Subscription(SubscriptionError::Validation(e.into())))?;

        self.schema_information
            .subscriptions
            .insert(id, subscription);
        self.modified = true;

        Ok(id)
    }

    pub fn remove_subscription(&mut self, subscription_id: SubscriptionId) {
        if self
            .schema_information
            .subscriptions
            .remove(&subscription_id)
            .is_some()
        {
            self.modified = true;
        }
    }

    pub fn modify_component(&mut self, component_name: String, public: bool) {
        if let Some(schemas) = self.schema_information.components.get_mut(&component_name) {
            // Update the public field
            if schemas.location.public != public {
                schemas.location.public = public;
                self.modified = true;
            }

            for h in schemas.handlers.values_mut() {
                if h.target_meta.public != public {
                    h.target_meta.public = public;
                    self.modified = true;
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DiscoveredHandlerMetadata {
    name: String,
    ty: HandlerType,
    input: InputRules,
    output: OutputRules,
}

impl DiscoveredHandlerMetadata {
    fn from_schema(
        component_type: ComponentType,
        handler: schema::Handler,
    ) -> Result<Self, ComponentError> {
        let handler_type = match handler.handler_type {
            None => HandlerType::default_for_component_type(component_type),
            Some(schema::HandlerType::Exclusive) => HandlerType::Exclusive,
            Some(schema::HandlerType::Shared) => HandlerType::Shared,
        };

        Ok(Self {
            name: handler.name.to_string(),
            ty: handler_type,
            input: handler
                .input
                .map(|s| DiscoveredHandlerMetadata::input_rules_from_schema(&handler.name, s))
                .transpose()?
                .unwrap_or_default(),
            output: handler
                .output
                .map(DiscoveredHandlerMetadata::output_rules_from_schema)
                .transpose()?
                .unwrap_or_default(),
        })
    }

    fn input_rules_from_schema(
        handler_name: &str,
        schema: schema::InputPayload,
    ) -> Result<InputRules, ComponentError> {
        let required = schema.required.unwrap_or(false);

        let mut input_validation_rules = vec![];

        // Add rule for empty if input not required
        if !required {
            input_validation_rules.push(InputValidationRule::NoBodyAndContentType);
        }

        // Add content-type validation rule
        let content_type = schema
            .content_type
            .map(|s| {
                s.parse()
                    .map_err(|e| ComponentError::BadInputContentType(handler_name.to_owned(), e))
            })
            .transpose()?
            .unwrap_or_default();
        if schema.json_schema.is_some() {
            input_validation_rules.push(InputValidationRule::JsonValue { content_type });
        } else {
            input_validation_rules.push(InputValidationRule::ContentType { content_type });
        }

        Ok(InputRules {
            input_validation_rules,
        })
    }

    fn output_rules_from_schema(
        schema: schema::OutputPayload,
    ) -> Result<OutputRules, ComponentError> {
        Ok(if let Some(ct) = schema.content_type {
            OutputRules {
                content_type_rule: OutputContentTypeRule::Set {
                    content_type: HeaderValue::from_str(&ct)
                        .map_err(|e| ComponentError::BadOutputContentType(ct, e))?,
                    set_content_type_if_empty: schema.set_content_type_if_empty.unwrap_or(false),
                    has_json_schema: schema.json_schema.is_some(),
                },
            }
        } else {
            OutputRules {
                content_type_rule: OutputContentTypeRule::None,
            }
        })
    }

    fn compute_handlers(
        component_ty: ComponentType,
        handlers: Vec<DiscoveredHandlerMetadata>,
    ) -> HashMap<String, HandlerSchemas> {
        handlers
            .into_iter()
            .map(|handler| {
                (
                    handler.name,
                    HandlerSchemas {
                        target_meta: InvocationTargetMetadata {
                            public: true,
                            component_ty,
                            handler_ty: handler.ty,
                            input_rules: handler.input,
                            output_rules: handler.output,
                        },
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use restate_schema_api::component::ComponentMetadataResolver;
    use restate_schema_api::deployment::{Deployment, DeploymentResolver};
    use restate_test_util::{assert, assert_eq, let_assert};

    use restate_types::Versioned;
    use test_log::test;

    const GREETER_SERVICE_NAME: &str = "greeter.Greeter";
    const ANOTHER_GREETER_SERVICE_NAME: &str = "greeter.AnotherGreeter";

    fn greeter_service() -> schema::Component {
        schema::Component {
            component_type: schema::ComponentType::Service,
            fully_qualified_component_name: GREETER_SERVICE_NAME.parse().unwrap(),
            handlers: vec![schema::Handler {
                name: "greet".parse().unwrap(),
                handler_type: None,
                input: None,
                output: None,
            }],
        }
    }

    fn greeter_virtual_object() -> schema::Component {
        schema::Component {
            component_type: schema::ComponentType::VirtualObject,
            fully_qualified_component_name: GREETER_SERVICE_NAME.parse().unwrap(),
            handlers: vec![schema::Handler {
                name: "greet".parse().unwrap(),
                handler_type: None,
                input: None,
                output: None,
            }],
        }
    }

    fn another_greeter_service() -> schema::Component {
        schema::Component {
            component_type: schema::ComponentType::Service,
            fully_qualified_component_name: ANOTHER_GREETER_SERVICE_NAME.parse().unwrap(),
            handlers: vec![schema::Handler {
                name: "another_greeter".parse().unwrap(),
                handler_type: None,
                input: None,
                output: None,
            }],
        }
    }

    #[test]
    fn register_new_deployment() {
        let schema_information = SchemaInformation::default();
        let initial_version = schema_information.version();
        let mut updater = SchemaUpdater::from(schema_information);

        let deployment = Deployment::mock();
        let deployment_id = updater
            .add_deployment(
                Some(deployment.id),
                deployment.metadata.clone(),
                vec![greeter_service()],
                false,
            )
            .unwrap();

        // Ensure we are using the pre-determined id
        assert_eq!(deployment.id, deployment_id);

        let schema = updater.into_inner();

        assert!(initial_version < schema.version());
        schema.assert_component_revision(GREETER_SERVICE_NAME, 1);
        schema.assert_component_deployment(GREETER_SERVICE_NAME, deployment_id);
        schema.assert_component_handler(GREETER_SERVICE_NAME, "greet");
    }

    #[test]
    fn register_new_deployment_add_unregistered_service() {
        let mut updater = SchemaUpdater::default();

        let deployment_1 = Deployment::mock_with_uri("http://localhost:9080");
        let deployment_2 = Deployment::mock_with_uri("http://localhost:9081");

        // Register first deployment
        updater
            .add_deployment(
                Some(deployment_1.id),
                deployment_1.metadata.clone(),
                vec![greeter_service()],
                false,
            )
            .unwrap();

        let schemas = updater.into_inner();

        schemas.assert_component_deployment(GREETER_SERVICE_NAME, deployment_1.id);
        assert!(schemas
            .resolve_latest_component(ANOTHER_GREETER_SERVICE_NAME)
            .is_none());

        updater = schemas.into();
        updater
            .add_deployment(
                Some(deployment_2.id),
                deployment_2.metadata.clone(),
                vec![greeter_service(), another_greeter_service()],
                false,
            )
            .unwrap();
        let schemas = updater.into_inner();

        schemas.assert_component_deployment(GREETER_SERVICE_NAME, deployment_2.id);
        schemas.assert_component_revision(GREETER_SERVICE_NAME, 2);
        schemas.assert_component_deployment(ANOTHER_GREETER_SERVICE_NAME, deployment_2.id);
        schemas.assert_component_revision(ANOTHER_GREETER_SERVICE_NAME, 1);
    }

    /// This test case ensures that https://github.com/restatedev/restate/issues/1205 works
    #[test]
    fn force_deploy_private_service() -> Result<(), SchemaError> {
        let mut updater = SchemaUpdater::default();
        let deployment = Deployment::mock();

        updater.add_deployment(
            Some(deployment.id),
            deployment.metadata.clone(),
            vec![greeter_service()],
            false,
        )?;

        let schemas = updater.into_inner();

        assert!(schemas.assert_component(GREETER_SERVICE_NAME).public);

        let version_before_modification = schemas.version();
        updater = SchemaUpdater::from(schemas);
        updater.modify_component(GREETER_SERVICE_NAME.to_owned(), false);
        let schemas = updater.into_inner();

        assert!(version_before_modification < schemas.version());
        assert!(!schemas.assert_component(GREETER_SERVICE_NAME).public);

        updater = SchemaUpdater::from(schemas);
        updater.add_deployment(
            Some(deployment.id),
            deployment.metadata.clone(),
            vec![greeter_service()],
            true,
        )?;

        let schemas = updater.into_inner();
        assert!(!schemas.assert_component(GREETER_SERVICE_NAME).public);

        Ok(())
    }

    mod change_instance_type {
        use super::*;

        use restate_test_util::assert;
        use test_log::test;

        #[test]
        fn register_new_deployment_fails_changing_instance_type() {
            let mut updater = SchemaUpdater::default();

            let deployment_1 = Deployment::mock_with_uri("http://localhost:9080");
            let deployment_2 = Deployment::mock_with_uri("http://localhost:9081");

            updater
                .add_deployment(
                    Some(deployment_1.id),
                    deployment_1.metadata.clone(),
                    vec![greeter_service()],
                    false,
                )
                .unwrap();
            let schemas = updater.into_inner();

            schemas.assert_component_deployment(GREETER_SERVICE_NAME, deployment_1.id);

            let compute_result = SchemaUpdater::from(schemas).add_deployment(
                Some(deployment_2.id),
                deployment_2.metadata,
                vec![greeter_virtual_object()],
                false,
            );

            assert!(let &SchemaError::Component(
                ComponentError::DifferentType(_)
            ) = compute_result.unwrap_err());
        }
    }

    #[test]
    fn override_existing_deployment_removing_a_service() {
        let mut updater = SchemaUpdater::default();

        let deployment = Deployment::mock();
        updater
            .add_deployment(
                Some(deployment.id),
                deployment.metadata.clone(),
                vec![greeter_service(), another_greeter_service()],
                false,
            )
            .unwrap();

        let schemas = updater.into_inner();
        schemas.assert_component_deployment(GREETER_SERVICE_NAME, deployment.id);
        schemas.assert_component_deployment(ANOTHER_GREETER_SERVICE_NAME, deployment.id);

        updater = SchemaUpdater::from(schemas);
        updater
            .add_deployment(
                Some(deployment.id),
                deployment.metadata.clone(),
                vec![greeter_service()],
                true,
            )
            .unwrap();

        let schemas = updater.into_inner();

        schemas.assert_component_deployment(GREETER_SERVICE_NAME, deployment.id);
        assert!(schemas
            .resolve_latest_component(ANOTHER_GREETER_SERVICE_NAME)
            .is_none());
    }

    #[test]
    fn cannot_override_existing_deployment_endpoint_conflict() {
        let mut updater = SchemaUpdater::default();

        let deployment = Deployment::mock();
        updater
            .add_deployment(
                Some(deployment.id),
                deployment.metadata.clone(),
                vec![greeter_service()],
                false,
            )
            .unwrap();

        assert!(let SchemaError::Override(_) = updater.add_deployment(
            Some(deployment.id),
            deployment.metadata,
            vec![greeter_service()],
            false).unwrap_err()
        );
    }

    #[test]
    fn cannot_override_existing_deployment_existing_id_mismatch() {
        let mut updater = SchemaUpdater::default();

        let deployment = Deployment::mock();
        updater
            .add_deployment(
                Some(deployment.id),
                deployment.metadata.clone(),
                vec![greeter_service()],
                false,
            )
            .unwrap();

        let new_id = DeploymentId::new();

        let rejection = updater
            .add_deployment(
                Some(new_id),
                deployment.metadata,
                vec![greeter_service()],
                false,
            )
            .unwrap_err();
        let_assert!(
            SchemaError::Deployment(DeploymentError::IncorrectId {
                requested,
                existing
            }) = rejection
        );
        assert_eq!(new_id, requested);
        assert_eq!(deployment.id, existing);
    }

    #[test]
    fn register_two_deployments_then_remove_first() {
        let mut updater = SchemaUpdater::default();

        let deployment_1 = Deployment::mock_with_uri("http://localhost:9080");
        let deployment_2 = Deployment::mock_with_uri("http://localhost:9081");

        updater
            .add_deployment(
                Some(deployment_1.id),
                deployment_1.metadata.clone(),
                vec![greeter_service(), another_greeter_service()],
                false,
            )
            .unwrap();
        updater
            .add_deployment(
                Some(deployment_2.id),
                deployment_2.metadata.clone(),
                vec![greeter_service()],
                false,
            )
            .unwrap();
        let schemas = updater.into_inner();

        schemas.assert_component_deployment(GREETER_SERVICE_NAME, deployment_2.id);
        schemas.assert_component_revision(GREETER_SERVICE_NAME, 2);
        schemas.assert_component_deployment(ANOTHER_GREETER_SERVICE_NAME, deployment_1.id);
        schemas.assert_component_revision(ANOTHER_GREETER_SERVICE_NAME, 1);

        let version_before_removal = schemas.version();
        updater = schemas.into();
        updater.remove_deployment(deployment_1.id);
        let schemas = updater.into_inner();

        schemas.assert_component_deployment(GREETER_SERVICE_NAME, deployment_2.id);
        schemas.assert_component_revision(GREETER_SERVICE_NAME, 2);
        assert!(version_before_removal < schemas.version());
        assert!(schemas
            .resolve_latest_component(ANOTHER_GREETER_SERVICE_NAME)
            .is_none());
        assert!(schemas.get_deployment(&deployment_1.id).is_none());
    }

    mod remove_method {
        use super::*;

        use restate_test_util::{check, let_assert};
        use test_log::test;

        fn greeter_v1_service() -> schema::Component {
            schema::Component {
                component_type: schema::ComponentType::Service,
                fully_qualified_component_name: GREETER_SERVICE_NAME.parse().unwrap(),
                handlers: vec![
                    schema::Handler {
                        name: "greet".parse().unwrap(),
                        handler_type: None,
                        input: None,
                        output: None,
                    },
                    schema::Handler {
                        name: "doSomething".parse().unwrap(),
                        handler_type: None,
                        input: None,
                        output: None,
                    },
                ],
            }
        }

        fn greeter_v2_service() -> schema::Component {
            schema::Component {
                component_type: schema::ComponentType::Service,
                fully_qualified_component_name: GREETER_SERVICE_NAME.parse().unwrap(),
                handlers: vec![schema::Handler {
                    name: "greet".parse().unwrap(),
                    handler_type: None,
                    input: None,
                    output: None,
                }],
            }
        }

        #[test]
        fn reject_removing_existing_methods() {
            let mut updater = SchemaUpdater::default();

            let deployment_1 = Deployment::mock_with_uri("http://localhost:9080");
            let deployment_2 = Deployment::mock_with_uri("http://localhost:9081");

            updater
                .add_deployment(
                    Some(deployment_1.id),
                    deployment_1.metadata,
                    vec![greeter_v1_service()],
                    false,
                )
                .unwrap();
            let schemas = updater.into_inner();
            schemas.assert_component_revision(GREETER_SERVICE_NAME, 1);

            updater = schemas.into();
            let rejection = updater
                .add_deployment(
                    Some(deployment_2.id),
                    deployment_2.metadata,
                    vec![greeter_v2_service()],
                    false,
                )
                .unwrap_err();

            let schemas = updater.into_inner();
            schemas.assert_component_revision(GREETER_SERVICE_NAME, 1); // unchanged

            let_assert!(
                SchemaError::Component(ComponentError::RemovedHandlers(service, missing_methods)) =
                    rejection
            );
            check!(service.as_ref() == GREETER_SERVICE_NAME);
            check!(missing_methods == &["doSomething"]);
        }
    }
}