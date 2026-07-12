//! CLI output rendering policy.

use anyhow::Result;

use crate::commands::{self, OutputFormat};

impl OutputFormat {
    pub(super) fn emit(self, value: &serde_json::Value) -> Result<()> {
        self.emit_json(value)
    }

    pub(super) fn render_artifact(
        self,
        artifact: &commands::artifacts::ProducedArtifact,
    ) -> Result<String> {
        match self {
            Self::Human => Ok(artifact.human_summary()),
            Self::Json | Self::Jsonl => Ok(serde_json::to_string(artifact)?),
        }
    }

    pub(super) fn emit_artifact(
        self,
        artifact: &commands::artifacts::ProducedArtifact,
    ) -> Result<()> {
        println!("{}", self.render_artifact(artifact)?);
        Ok(())
    }
}

#[cfg(test)]
mod focused_tests {
    use std::path::PathBuf;

    use super::*;

    fn artifact() -> commands::artifacts::ProducedArtifact {
        commands::artifacts::ProducedArtifact::new(
            commands::artifacts::PublishedArtifact {
                path: PathBuf::from("/tmp/example.png"),
                bytes: 42,
            },
            PathBuf::from("example.png"),
            commands::artifacts::HumanArtifactOutput::Saved,
            "image/png",
            Some((10, 20)),
            commands::artifacts::ArtifactContext::default(),
        )
    }

    #[test]
    fn artifact_rendering_preserves_human_json_and_jsonl_policy() {
        assert_eq!(
            OutputFormat::Human.render_artifact(&artifact()).unwrap(),
            "saved example.png"
        );
        let json = OutputFormat::Json.render_artifact(&artifact()).unwrap();
        assert!(!json.contains('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["schemaVersion"],
            1
        );
        let jsonl = OutputFormat::Jsonl.render_artifact(&artifact()).unwrap();
        assert!(!jsonl.contains('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&jsonl).unwrap()["kind"],
            "artifact"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::arguments::STRUCTURED_COMMAND_INVENTORY;
    use std::path::PathBuf;
    fn output_artifact() -> commands::artifacts::ProducedArtifact {
        commands::artifacts::ProducedArtifact::new(
            commands::artifacts::PublishedArtifact {
                path: PathBuf::from("/tmp/example.png"),
                bytes: 42,
            },
            PathBuf::from("example.png"),
            commands::artifacts::HumanArtifactOutput::Saved,
            "image/png",
            Some((10, 20)),
            commands::artifacts::ArtifactContext::default(),
        )
    }

    #[test]
    fn documented_structured_schema_contract_matrix() {
        let contracts: Vec<(&str, serde_json::Value, &str)> = vec![
            (
                "status",
                serde_json::json!({"schemaVersion":1,"kind":"status","status":"missing","healthy":false}),
                "no session",
            ),
            (
                "list",
                serde_json::json!({"schemaVersion":1,"kind":"list","instances":[]}),
                "",
            ),
            (
                "cleanup",
                serde_json::json!({"schemaVersion":1,"kind":"cleanup","results":[{"dir":"/state/a","pid":null,"label":null,"action":"cleaned","reason":"cleaned"},{"dir":"/state/b","pid":42,"label":"b","action":"preserved_inconclusive_timeout","reason":"probe timed out"}]}),
                "cleaned: /state/a",
            ),
            (
                "open",
                serde_json::json!({"schemaVersion":1,"kind":"open","url":"https://example.com/"}),
                "Example — https://example.com/",
            ),
            (
                "cookie list",
                serde_json::json!({"schemaVersion":1,"kind":"cookies","url":"https://example.com/","cookies":[]}),
                "",
            ),
            (
                "viewport",
                serde_json::json!({"schemaVersion":1,"kind":"viewport","applied":false,"width":800,"height":600,"scale":1.0,"mobile":false}),
                "800x600",
            ),
            (
                "logs",
                serde_json::json!({"schemaVersion":1,"kind":"log","timestamp":1.0,"instance":"inst","target":"target","cdpSession":"cdp","severity":"log","message":"[log] hi"}),
                "[log] hi",
            ),
            (
                "pages",
                serde_json::json!({"schemaVersion":1,"kind":"pages","pages":[{"index":0,"current":true,"id":"target","url":"https://example.com/","title":"Example"},{"index":1,"current":false,"id":"target-2","url":"about:blank","title":""}]}),
                "* 0: https://example.com/ (Example)",
            ),
            (
                "screenshot",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "screenshot-el",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "pdf",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "download FILE",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
            (
                "stop-video",
                serde_json::from_str(
                    &OutputFormat::Jsonl
                        .render_artifact(&output_artifact())
                        .unwrap(),
                )
                .unwrap(),
                "saved example.png",
            ),
        ];
        assert_eq!(
            contracts
                .iter()
                .map(|(name, _, _)| *name)
                .collect::<Vec<_>>()
                .join(", "),
            STRUCTURED_COMMAND_INVENTORY
        );
        for (name, value, human_hint) in contracts {
            assert_eq!(value["schemaVersion"], 1, "{name}");
            assert!(value["kind"].is_string(), "{name}");
            let compact = OutputFormat::Json.render_json(&value).unwrap();
            assert!(!compact.contains('\n'), "{name} json should be compact");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&compact).unwrap(),
                value
            );
            let jsonl = OutputFormat::Jsonl.render_json(&value).unwrap();
            assert!(
                !jsonl.contains('\n'),
                "{name} jsonl should be compact one record"
            );
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&jsonl).unwrap(),
                value
            );
            if !human_hint.is_empty() {
                assert!(!human_hint.contains('\u{1b}'), "{name} human stdout purity");
            }
        }

        let log_one = serde_json::json!({"schemaVersion":1,"kind":"log","timestamp":1.0,"target":"t","severity":"log","message":"one"});
        let log_two = serde_json::json!({"schemaVersion":1,"kind":"log","timestamp":2.0,"target":"t","severity":"error","message":"two"});
        let stream = format!(
            "{}\n{}\n",
            OutputFormat::Jsonl.render_json(&log_one).unwrap(),
            OutputFormat::Jsonl.render_json(&log_two).unwrap()
        );
        assert_eq!(
            stream.lines().count(),
            2,
            "multi-event logs are one record per line"
        );
        let empty_finite = OutputFormat::Jsonl
            .render_json(&serde_json::json!({"schemaVersion":1,"kind":"list","instances":[]}))
            .unwrap();
        assert!(empty_finite.contains("\"instances\":[]"));
        let multi_finite = OutputFormat::Jsonl.render_json(&serde_json::json!({"schemaVersion":1,"kind":"pages","pages":[{"index":0},{"index":1}]})).unwrap();
        assert_eq!(
            multi_finite.lines().count(),
            1,
            "finite multi-item jsonl stays one envelope"
        );
    }

    #[test]
    fn artifact_output_formats_are_executable_schema_snapshots() {
        let artifact = output_artifact();
        assert_eq!(
            OutputFormat::Human.render_artifact(&artifact).unwrap(),
            "saved example.png"
        );

        let json = OutputFormat::Json.render_artifact(&artifact).unwrap();
        assert!(!json.contains('\n'));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::json!({
                "schemaVersion": 1,
                "kind": "artifact",
                "path": "/tmp/example.png",
                "type": "image/png",
                "bytes": 42,
                "width": 10,
                "height": 20
            })
        );

        let jsonl = OutputFormat::Jsonl.render_artifact(&artifact).unwrap();
        assert_eq!(
            jsonl,
            r#"{"schemaVersion":1,"kind":"artifact","path":"/tmp/example.png","type":"image/png","bytes":42,"width":10,"height":20}"#
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&jsonl).unwrap()["kind"],
            "artifact"
        );
    }
}
