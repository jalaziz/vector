use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use vector_core::transform::{InnerTopology, InnerTopologyTransform};

use crate::{
    conditions::{AnyCondition, Condition},
    config::{ComponentKey, DataType, Input, Output, TransformConfig, TransformContext},
    event::Event,
    schema,
    transforms::{SyncTransform, Transform, TransformOutputsBuf},
};

//------------------------------------------------------------------------------

/// This represents the configuration of a single pipeline, not the pipelines transform
/// itself, which can contain multiple individual pipelines
#[derive(Debug, Default, Deserialize, Serialize)]
pub(crate) struct PipelineConfig {
    name: String,
    filter: Option<AnyCondition>,
    #[serde(default)]
    transforms: Vec<Box<dyn TransformConfig>>,
}

#[cfg(test)]
impl PipelineConfig {
    #[allow(dead_code)] // for some small subset of feature flags this code is dead
    pub(crate) fn transforms(&self) -> &Vec<Box<dyn TransformConfig>> {
        &self.transforms
    }
}

impl Clone for PipelineConfig {
    fn clone(&self) -> Self {
        // This is a hack around the issue of cloning
        // trait objects. So instead to clone the config
        // we first serialize it into JSON, then back from
        // JSON. Originally we used TOML here but TOML does not
        // support serializing `None`.
        let json = serde_json::to_value(self).unwrap();
        serde_json::from_value(json).unwrap()
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "pipeline")]
impl TransformConfig for PipelineConfig {
    async fn build(&self, ctx: &TransformContext) -> crate::Result<Transform> {
        let condition = if let Some(config) = &self.filter {
            Some(config.build(&ctx.enrichment_tables)?)
        } else {
            None
        };

        let mut transforms = Vec::new();
        for config in &self.transforms {
            let transform = match config.build(ctx).await? {
                Transform::Function(transform) => Box::new(transform),
                Transform::Synchronous(transform) => transform,
                _ => panic!("non-sync transform in pipeline: {:?}", config),
            };
            transforms.push(transform);
        }
        Ok(Transform::Synchronous(Box::new(Pipeline {
            condition,
            transforms,
        })))
    }

    fn input(&self) -> Input {
        Input::all()
    }

    fn outputs(&self, _: &schema::Definition) -> Vec<Output> {
        vec![Output::default(DataType::all())]
    }

    fn transform_type(&self) -> &'static str {
        "pipeline"
    }

    fn enable_concurrency(&self) -> bool {
        true
    }
}

impl PipelineConfig {
    pub(super) fn expand(
        &mut self,
        name: &ComponentKey,
        inputs: &[String],
    ) -> crate::Result<Option<InnerTopology>> {
        let mut result = InnerTopology::default();

        result.inner.insert(
            name.clone(),
            InnerTopologyTransform {
                inputs: inputs.to_vec(),
                inner: Box::new(self.clone()),
            },
        );
        // TODO: actually call outputs fn
        result
            .outputs
            .push((name.clone(), vec![Output::default(DataType::all())]));

        Ok(Some(result))
    }
}

#[derive(Clone)]
struct Pipeline {
    condition: Option<Condition>,
    transforms: Vec<Box<dyn SyncTransform>>,
}

impl SyncTransform for Pipeline {
    fn transform(&mut self, event: Event, output: &mut TransformOutputsBuf) {
        if let Some(condition) = &self.condition {
            if condition.check(&event) == false {
                output.push(event);
                return;
            }
        }
        let mut alt = TransformOutputsBuf::dummy();
        output.push(event);
        for transform in &mut self.transforms {
            std::mem::swap(&mut alt, output);
            for event in alt.primary_buffer.as_mut().unwrap().drain() {
                transform.transform(event, output);
            }
        }
    }
}

//------------------------------------------------------------------------------

/// This represent an ordered list of pipelines depending on the event type.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct EventTypeConfig(Vec<PipelineConfig>);

impl AsRef<Vec<PipelineConfig>> for EventTypeConfig {
    fn as_ref(&self) -> &Vec<PipelineConfig> {
        &self.0
    }
}

impl EventTypeConfig {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn validate_nesting(&self, parents: &HashSet<&'static str>) -> Result<(), String> {
        for (pipeline_index, pipeline) in self.0.iter().enumerate() {
            let pipeline_name = pipeline.name.as_str();
            for (transform_index, transform) in pipeline.transforms.iter().enumerate() {
                if !transform.nestable(parents) {
                    return Err(format!(
                        "the transform {} in pipeline {:?} (at index {}) cannot be nested in {:?}",
                        transform_index, pipeline_name, pipeline_index, parents
                    ));
                }
            }
        }
        Ok(())
    }
}

impl EventTypeConfig {
    /// Expand sub-pipelines configurations, preserving user defined order
    ///
    /// This function expands the sub-pipelines according to the order passed by
    /// the user, or, absent an explicit order, by the position of the
    /// sub-pipeline in the configuration file.
    pub(super) fn expand(
        &mut self,
        name: &ComponentKey,
        inputs: &[String],
    ) -> crate::Result<Option<InnerTopology>> {
        let mut result = InnerTopology::default();
        let mut next_inputs = inputs.to_vec();
        for (pipeline_index, pipeline_config) in self.0.iter_mut().enumerate() {
            let pipeline_name = name.join(pipeline_index);
            let topology = pipeline_config
                .expand(&pipeline_name, &next_inputs)?
                .ok_or_else(|| {
                    format!(
                        "Unable to expand pipeline {:?} ({:?})",
                        pipeline_config.name, pipeline_name
                    )
                })?;
            result.inner.extend(topology.inner.into_iter());
            result.outputs = topology.outputs;
            next_inputs = result.outputs();
        }
        //
        Ok(Some(result))
    }
}
