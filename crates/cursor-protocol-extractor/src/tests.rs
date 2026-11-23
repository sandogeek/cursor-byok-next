//! Ports of the upstream Go extractor tests that local loop still needs.

use std::collections::HashMap;

use super::{
    BundleScan, Enum, Field, FieldKind, Message, Service, Strictness, SymbolKind, TypeContext,
    TypeToken, ValueKind, declaration_coverage, extract_from_sources, has_placeholder,
    merge_messages_by_type_name, parse_field_object_source, reclassify_enum_fields,
    resolver_from_scan, scan_source, validate_required_agent_shapes,
};

fn field_by_name<'a>(fields: &'a [Field], name: &str) -> Option<&'a Field> {
    fields.iter().find(|field| field.name == name)
}

/// A compact-syntax bundle declaring the `RunInference` mode-config shapes.
const RUN_INFERENCE_BUNDLE: &str = r#"
D6t="aiserver.v1.";
class F6t extends Base{static $p(){return D6t}}
K6t=(0,s.QT)(r.proto3,D6t,"InferenceMessageRole",[[0,"UNSPECIFIED"],[1,"USER"],[2,"ASSISTANT"],[3,"TOOL"],[4,"SYSTEM"]],1);
class z7t extends F6t{
  static $(){return["RunInferenceClientMessage|1 run_request #0 message|4 finish_run #1 message",Qp1,lf1]}
}
class vf1 extends F6t{
  static $(){return["RunInferencePromptModelMetadata|1 vendor 9|36 feature_flags 9,#0|46 agent_mode_configs #1*|48 ask_question_config #2?",Tf1,Cf1,wf1]}
}
class Cf1 extends F6t{
  static $(){return["RunInferenceAgentModeConfig|1 mode_id 9|2 enable_ask_question_tool 8|3 switch_mode_tool_config #0?",kf1]}
}
class wf1 extends F6t{
  static $(){return["RunInferenceAskQuestionConfig|1 tool_name 9|2 auto_reject_first 8"]}
}
class Tf1 extends F6t{
  static $(){return["RunInferenceFlagValue|1 bool_value 8 value|2 number_value 1 value|3 string_value 9 value"]}
}
class kf1 extends F6t{
  static $(){return["RunInferenceSwitchModeToolConfig|1 target_modes 9*"]}
}
class Lp1 extends F6t{
  static $(){return["InferenceToolResultPart|1 tool_call_id 9|3 result #0",r.Value]}
}
class hf1 extends F6t{
  static $(){return["RunInferenceRunReady|12 metric_labels 9,9|4 prompt_model_metadata #0",vf1]}
}
InferenceReason=enumType(proto3,__protoPackage117,"InferenceReason",[[0,"UNSPECIFIED"],[1,"GEMINI_VIDEO_SUBAGENT"]],1);
__protoPackage117="aiserver.v1.";
var Pf1={typeName:"aiserver.v1.InferenceService",methods:{runInference:{name:"RunInference",I:z7t,O:hf1,kind:ye.BiDiStreaming},runInferenceSSE:{name:"RunInferenceSSE",I:j$e,O:hf1,kind:ye.ServerStreaming},recordEmpty:{name:"RecordEmpty",I:z7t,O:r.Empty,kind:ye.Unary}}};
"#;

fn enum_by_name<'a>(enums: &'a [Enum], type_name: &str) -> Option<&'a Enum> {
    enums.iter().find(|item| item.type_name == type_name)
}

#[test]
fn parse_field_object_supports_shorthand_type() {
    let field = parse_field_object_source(
        r#"{no:4,name:"file_not_found",kind:"message",T,oneof:"result"}"#,
    )
    .expect("parse shorthand T");
    assert_eq!(field.t, Some(TypeToken::Named("T".into())));
}

#[test]
fn webpack_export_alias_resolves_service_message_type() {
    const BUNDLE: &str = r#"
1:(e,t,n)=>{
  n.d(t,{KS:()=>T,_B:()=>r});
  var r;
  class T {}
  T.typeName="agent.v1.AgentClientMessage";
  n.proto3.util.setEnumType(r,"agent.v1.DiagnosticSeverity",[]);
},
2:(e,t,n)=>{
  var r=n(1);
  const service={typeName:"agent.v1.AgentService",methods:{run:{name:"Run",I:r.KS,O:r.KS,kind:n.MethodKind.BiDiStreaming}}};
}
"#;
    let scan = scan_source(BUNDLE);
    let resolver = resolver_from_scan(&scan);
    let service = scan
        .services
        .iter()
        .find(|service| service.type_name == "agent.v1.AgentService")
        .expect("AgentService");
    let type_name = resolver
        .resolve_type_name(
            "r.KS",
            TypeContext::new("agent.v1", scan.source_end(), service.module_start),
            SymbolKind::Message,
        )
        .or_else(|| {
            resolver.resolve_type_name(
                "r.KS",
                TypeContext::new("agent.v1", u32::MAX, service.module_start),
                SymbolKind::Message,
            )
        });
    assert_eq!(type_name.as_deref(), Some("agent.v1.AgentClientMessage"));
}

#[test]
fn resolver_prefers_expected_kind_over_current_package() {
    let resolver = super::TypeResolver::new(
        &[Message {
            type_name: "git_forge.v1.GetTagResponse".into(),
            var_name: "nt".into(),
            internal_name: String::new(),
            fields: Vec::new(),
            package: "git_forge.v1".into(),
            short_name: "GetTagResponse".into(),
            pos: 10,
            module_start: Some(1),
        }],
        &[Enum {
            type_name: "origin.v1.TeamGroupKind".into(),
            var_name: "nt".into(),
            values: Vec::new(),
            package: "origin.v1".into(),
            short_name: "TeamGroupKind".into(),
            pos: 20,
            module_start: Some(1),
        }],
        &HashMap::default(),
        &HashMap::default(),
        HashMap::default(),
    );
    let type_name = resolver
        .resolve_type_name(
            "nt",
            TypeContext::new("origin.v1", 30, Some(1)),
            SymbolKind::Message,
        )
        .expect("cross-package message");
    assert_eq!(type_name, "git_forge.v1.GetTagResponse");
}

#[test]
fn modern_factory_syntax_extracts_in_app_ad_service_types() {
    const BUNDLE: &str = r#"
42:(e,t,n)=>{
  var HasSeenAdRequest=n.makeMessageType("aiserver.v1.HasSeenAdRequest",()=>[{no:1,name:"ad_id",kind:"scalar",T:9}]),
      HasSeenAdResponse=n.makeMessageType("aiserver.v1.HasSeenAdResponse",()=>[{no:1,name:"has_seen",kind:"scalar",T:8}]),
      MarkAdAsSeenResponse=n.makeMessageType("aiserver.v1.MarkAdAsSeenResponse",[]),
      Placement=n.makeEnum("aiserver.v1.InAppAdPlacement",[{no:0,name:"IN_APP_AD_PLACEMENT_UNSPECIFIED",localName:"UNSPECIFIED"}]),
      InAppAdService={typeName:"aiserver.v1.InAppAdService",methods:{hasSeenAd:{name:"HasSeenAd",I:HasSeenAdRequest,O:HasSeenAdResponse,kind:n.MethodKind.Unary},markAdAsSeen:{name:"MarkAdAsSeen",I:HasSeenAdRequest,O:MarkAdAsSeenResponse,kind:n.MethodKind.Unary}}};
}
"#;
    let scan = scan_source(BUNDLE);
    assert_eq!(
        scan.messages.len(),
        3,
        "messages: {:?}",
        scan.messages
            .iter()
            .map(|m| (m.type_name.clone(), m.var_name.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(scan.messages[0].fields[0].name, "ad_id");
    assert_eq!(scan.enums.len(), 1);
    assert_eq!(scan.enums[0].type_name, "aiserver.v1.InAppAdPlacement");
    assert_eq!(scan.services.len(), 1);
    assert_eq!(scan.services[0].methods.len(), 2);
    let resolver = resolver_from_scan(&scan);
    let method = &scan.services[0].methods[0];
    let service = &scan.services[0];
    let context = TypeContext::new(&service.package, service.pos, service.module_start);
    let input = resolver
        .resolve_type_name(&method.input_type, context, SymbolKind::Message)
        .expect("input");
    let output = resolver
        .resolve_type_name(&method.output_type, context, SymbolKind::Message)
        .expect("output");
    assert_eq!(input, "aiserver.v1.HasSeenAdRequest");
    assert_eq!(output, "aiserver.v1.HasSeenAdResponse");
}

#[test]
fn resolver_maps_dotted_google_well_known_types() {
    let resolver = super::TypeResolver::new(
        &[],
        &[],
        &HashMap::default(),
        &HashMap::default(),
        HashMap::default(),
    );
    assert_eq!(
        resolver
            .resolve_type_name(
                "r.Empty",
                TypeContext::new("aiserver.v1", 0, None),
                SymbolKind::Message
            )
            .as_deref(),
        Some("google.protobuf.Empty")
    );
    assert_eq!(
        resolver
            .resolve_type_name(
                "r.Value",
                TypeContext::new("aiserver.v1", 0, None),
                SymbolKind::Message
            )
            .as_deref(),
        Some("google.protobuf.Value")
    );
}

#[test]
fn assignment_alias_resolves_standard_protobuf_type() {
    const BUNDLE: &str = r#"
1:(e,t,n)=>{
  var Timestamp=class TimestampMessage extends Base{};
  Timestamp.typeName="google.protobuf.Timestamp",Timestamp.fields=n.proto3.util.newFieldList(()=>[]),ua=Timestamp;
  var Request=n.makeMessageType("aiserver.v1.Request",()=>[{no:1,name:"created_at",kind:"message",T:ua}]);
}
"#;
    let scan = scan_source(BUNDLE);
    let resolver = resolver_from_scan(&scan);
    let type_name = resolver
        .resolve_type_name(
            "ua",
            TypeContext::new(
                "aiserver.v1",
                u32::MAX,
                scan.messages
                    .last()
                    .and_then(|message| message.module_start),
            ),
            SymbolKind::Message,
        )
        .expect("alias");
    assert_eq!(type_name, "google.protobuf.Timestamp");
}

#[test]
fn declaration_coverage_reports_unparsed_types_and_ignores_google_types() {
    const BUNDLE: &str = r#"
var Request=n.makeMessageType("aiserver.v1.Request",()=>[]);
var Missing=n.makeMessageType("aiserver.v1.Missing",()=>[]);
var Timestamp=n.makeMessageType("google.protobuf.Timestamp",()=>[]);
var Service={typeName:"aiserver.v1.TestService",methods:{}};
"#;
    let scan = scan_source(BUNDLE);
    let messages = vec![Message {
        type_name: "aiserver.v1.Request".into(),
        var_name: String::new(),
        internal_name: String::new(),
        fields: Vec::new(),
        package: "aiserver.v1".into(),
        short_name: "Request".into(),
        pos: 0,
        module_start: None,
    }];
    let services = vec![Service {
        type_name: "aiserver.v1.TestService".into(),
        var_name: String::new(),
        methods: Vec::new(),
        package: "aiserver.v1".into(),
        short_name: "TestService".into(),
        pos: 0,
        module_start: None,
    }];
    let coverage = declaration_coverage(&scan.declared, &messages, &[], &services);
    assert_eq!((coverage.declared, coverage.extracted), (3, 2));
    assert_eq!(coverage.missing, vec!["aiserver.v1.Missing".to_string()]);
}

#[test]
fn extract_services_supports_anonymous_descriptors() {
    const BUNDLE: &str = r#"services.push({typeName:"aiserver.v1.FileSyncService",methods:{sync:{name:"Sync",I:Request,O:Response,kind:n.MethodKind.Unary}}})"#;
    let scan = scan_source(BUNDLE);
    assert_eq!(scan.services.len(), 1);
    assert_eq!(scan.services[0].type_name, "aiserver.v1.FileSyncService");
    assert_eq!(scan.services[0].methods[0].name, "Sync");
}

#[test]
fn blob_metadata_type_name_is_not_a_proto_declaration() {
    const BUNDLE: &str = r#"
getBlobMetadataCallback({blobType:{kind:"proto",typeName:"agent.v1.RedactedConversationStateStructure"}});
var Keep=n.makeMessageType("agent.v1.Keep",()=>[]);
"#;
    let scan = scan_source(BUNDLE);
    assert!(
        !scan
            .declared
            .iter()
            .any(|name| name == "agent.v1.RedactedConversationStateStructure")
    );
    assert!(scan.declared.iter().any(|name| name == "agent.v1.Keep"));
}

#[test]
fn real_unknown_prefixed_field_names_are_not_placeholders() {
    assert!(!has_placeholder(
        "  string unknown_tool_error_template = 1;"
    ));
    assert!(has_placeholder("  string unknown = 1;"));
    assert!(has_placeholder("  bytes field_12 = 12;"));
}

#[test]
fn merge_messages_prefers_primary_bundle_and_keeps_supplemental_types() {
    let merged = merge_messages_by_type_name(vec![
        Message {
            type_name: "aiserver.v1.Shared".into(),
            var_name: String::new(),
            internal_name: String::new(),
            fields: vec![Field::new(1, "primary", FieldKind::Scalar)],
            package: "aiserver.v1".into(),
            short_name: "Shared".into(),
            pos: 0,
            module_start: None,
        },
        Message {
            type_name: "aiserver.v1.Shared".into(),
            var_name: String::new(),
            internal_name: String::new(),
            fields: vec![Field::new(1, "supplemental", FieldKind::Scalar)],
            package: "aiserver.v1".into(),
            short_name: "Shared".into(),
            pos: 0,
            module_start: None,
        },
        Message {
            type_name: "aiserver.v1.LegacyOnly".into(),
            var_name: String::new(),
            internal_name: String::new(),
            fields: Vec::new(),
            package: "aiserver.v1".into(),
            short_name: "LegacyOnly".into(),
            pos: 0,
            module_start: None,
        },
    ]);
    assert_eq!(merged.len(), 2);
    assert_eq!(merged[0].fields.len(), 1);
    assert_eq!(merged[1].type_name, "aiserver.v1.LegacyOnly");
}

#[test]
fn merge_messages_prefers_qualified_type_references() {
    let unresolved = Message {
        type_name: "aiserver.v1.BuiltinToolResult".into(),
        var_name: String::new(),
        internal_name: String::new(),
        fields: vec![Field {
            t: Some(TypeToken::Named("uR".into())),
            oneof: Some("result".into()),
            ..Field::new(5, "edit_result", FieldKind::Message)
        }],
        package: "aiserver.v1".into(),
        short_name: "BuiltinToolResult".into(),
        pos: 0,
        module_start: None,
    };
    let qualified = Message {
        type_name: "aiserver.v1.BuiltinToolResult".into(),
        var_name: String::new(),
        internal_name: String::new(),
        fields: vec![Field {
            t: Some(TypeToken::Named("aiserver.v1.EditResult".into())),
            oneof: Some("result".into()),
            ..Field::new(5, "edit_result", FieldKind::Message)
        }],
        package: "aiserver.v1".into(),
        short_name: "BuiltinToolResult".into(),
        pos: 0,
        module_start: None,
    };
    let merged = merge_messages_by_type_name(vec![unresolved, qualified]);
    assert_eq!(
        merged[0].fields[0].t,
        Some(TypeToken::Named("aiserver.v1.EditResult".into()))
    );
}

#[test]
fn merge_messages_prefers_more_complete_definition() {
    let subset = Message {
        type_name: "aiserver.v1.RunInferencePromptModelMetadata".into(),
        var_name: String::new(),
        internal_name: String::new(),
        fields: vec![Field::new(1, "vendor", FieldKind::Scalar)],
        package: "aiserver.v1".into(),
        short_name: "RunInferencePromptModelMetadata".into(),
        pos: 0,
        module_start: None,
    };
    let complete = Message {
        type_name: "aiserver.v1.RunInferencePromptModelMetadata".into(),
        var_name: String::new(),
        internal_name: String::new(),
        fields: vec![
            Field::new(1, "vendor", FieldKind::Scalar),
            Field {
                t: Some(TypeToken::Named(
                    "aiserver.v1.RunInferenceAgentModeConfig".into(),
                )),
                repeated: true,
                ..Field::new(46, "agent_mode_configs", FieldKind::Message)
            },
            Field {
                t: Some(TypeToken::Named(
                    "aiserver.v1.RunInferenceAskQuestionConfig".into(),
                )),
                opt: true,
                ..Field::new(48, "ask_question_config", FieldKind::Message)
            },
        ],
        package: "aiserver.v1".into(),
        short_name: "RunInferencePromptModelMetadata".into(),
        pos: 0,
        module_start: None,
    };
    let merged = merge_messages_by_type_name(vec![subset, complete]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].fields.len(), 3);
}

#[test]
fn compact_factory_extracts_run_inference_mode_config() {
    let mut scan = scan_run_inference_bundle();
    let by_type = messages_by_type(&scan);
    assert_run_inference_metadata(&by_type);
    assert_run_inference_maps(&by_type);
    let role = enum_by_name(&scan.enums, "aiserver.v1.InferenceMessageRole").expect("role");
    assert_eq!(role.values.len(), 5);
    assert_eq!(role.values[1].name, "INFERENCE_MESSAGE_ROLE_USER");
    let reason = enum_by_name(&scan.enums, "aiserver.v1.InferenceReason").expect("reason");
    assert_eq!(
        reason.values[1].name,
        "INFERENCE_REASON_GEMINI_VIDEO_SUBAGENT"
    );
    assert_eq!(scan.services.len(), 1);
    assert_eq!(scan.services[0].type_name, "aiserver.v1.InferenceService");
    let resolver = resolver_from_scan(&scan);
    super::normalize_type_references(&mut scan.messages, &mut scan.services, &resolver);
    assert_eq!(
        scan.services[0].methods[2].output_type,
        "google.protobuf.Empty"
    );
}

fn scan_run_inference_bundle() -> BundleScan {
    let mut scan = scan_source(RUN_INFERENCE_BUNDLE);
    reclassify_enum_fields(&mut scan.messages, &scan.enums);
    scan
}

fn messages_by_type(scan: &BundleScan) -> HashMap<String, Message> {
    scan.messages
        .iter()
        .map(|message| (message.type_name.clone(), message.clone()))
        .collect()
}

fn assert_run_inference_metadata(by_type: &HashMap<String, Message>) {
    let meta = by_type
        .get("aiserver.v1.RunInferencePromptModelMetadata")
        .expect("metadata");
    let field = field_by_name(&meta.fields, "agent_mode_configs").expect("agent_mode_configs");
    assert!(field.repeated);
    assert_eq!(
        field.t,
        Some(TypeToken::Named(
            "aiserver.v1.RunInferenceAgentModeConfig".into()
        ))
    );
    let ask = field_by_name(&meta.fields, "ask_question_config").expect("ask_question_config");
    assert!(ask.opt);
    assert_eq!(
        ask.t,
        Some(TypeToken::Named(
            "aiserver.v1.RunInferenceAskQuestionConfig".into()
        ))
    );
    let mode = by_type
        .get("aiserver.v1.RunInferenceAgentModeConfig")
        .expect("mode config");
    assert!(field_by_name(&mode.fields, "mode_id").is_some());
    assert!(field_by_name(&mode.fields, "enable_ask_question_tool").is_some());
}

fn assert_run_inference_maps(by_type: &HashMap<String, Message>) {
    let result_part = by_type
        .get("aiserver.v1.InferenceToolResultPart")
        .expect("result part");
    let result = field_by_name(&result_part.fields, "result").expect("result");
    assert_eq!(
        result.t,
        Some(TypeToken::Named("google.protobuf.Value".into()))
    );
    let ready = by_type
        .get("aiserver.v1.RunInferenceRunReady")
        .expect("run ready");
    let labels = field_by_name(&ready.fields, "metric_labels").expect("metric_labels");
    assert_eq!(labels.kind, FieldKind::Map);
    assert_eq!(labels.map_key, Some(9));
    assert_eq!(labels.map_value_kind, ValueKind::Scalar);
}

#[test]
fn validate_required_agent_shapes_accepts_run_inference_mode_config() {
    const BODY: &str = r"
message RunInferenceAgentModeConfig {
  string mode_id = 1;
  bool enable_ask_question_tool = 2;
  optional RunInferenceSwitchModeToolConfig switch_mode_tool_config = 3;
}
message RunInferencePromptModelMetadata {
  repeated RunInferenceAgentModeConfig agent_mode_configs = 46;
  optional RunInferenceAskQuestionConfig ask_question_config = 48;
}
service InferenceService {
  rpc RunInference(stream RunInferenceClientMessage) returns (stream RunInferenceServerMessage) {}
  rpc RunInferenceSSE(BidiRequestId) returns (stream RunInferenceServerMessage) {}
}
";
    validate_required_agent_shapes("aiserver_v1.proto", BODY).expect("valid shapes");
}

#[test]
fn validate_required_agent_shapes_stays_inside_message_block() {
    const BODY: &str = r"
message RunInferencePromptModelMetadata {
  string vendor = 1;
}
message Other {
  repeated RunInferenceAgentModeConfig agent_mode_configs = 46;
  optional RunInferenceAskQuestionConfig ask_question_config = 48;
}
";
    let err =
        validate_required_agent_shapes("aiserver_v1.proto", BODY).expect_err("cross-message leak");
    assert!(err.to_string().contains("agent_mode_configs"), "got {err}");
}

#[test]
fn scan_records_oxc_parse_errors() {
    let scan = scan_source("var x = {;");
    assert_ne!(scan.diagnostics.parse_errors, Vec::<String>::new());
}

#[test]
fn scan_keeps_valid_factory_after_recoverable_noise() {
    let scan = scan_source("var Keep=n.makeMessageType(\"agent.v1.Keep\",()=>[]);");
    assert!(
        scan.messages
            .iter()
            .any(|message| message.type_name == "agent.v1.Keep")
    );
}

#[test]
fn no_strict_extract_keeps_files_when_required_shapes_are_missing() {
    const BUNDLE: &str = r#"
var Keep=n.makeMessageType("aiserver.v1.Keep",()=>[{no:1,name:"id",kind:"scalar",T:9}]);
var Ads={typeName:"aiserver.v1.InAppAdService",methods:{ping:{name:"Ping",I:Keep,O:Keep,kind:n.MethodKind.Unary}}};
"#;
    let result = extract_from_sources(&[BUNDLE.to_string()], Strictness::Lenient).expect("extract");
    assert!(result.files.contains_key("aiserver_v1.proto"));
    assert_eq!(result.diagnostics.shape_errors, Vec::<String>::new());
}

#[test]
fn write_protos_replaces_directory_atomically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("out");
    std::fs::create_dir_all(&output).expect("create output");
    std::fs::write(output.join("stale.proto"), "old").expect("stale");
    let mut files = std::collections::HashMap::new();
    files.insert("agent_v1.proto".into(), "syntax = \"proto3\";\n".into());
    super::write_protos(&output, &files).expect("write");
    assert!(output.join("agent_v1.proto").exists());
    assert!(!output.join("stale.proto").exists());
}

#[test]
fn local_loop_extract_drops_unrelated_packages() {
    const BUNDLE: &str = r#"
var Keep=n.makeMessageType("agent.v1.Keep",()=>[{no:1,name:"id",kind:"scalar",T:9}]);
var Drop=n.makeMessageType("origin.v1.Drop",()=>[{no:1,name:"id",kind:"scalar",T:9}]);
var Ads={typeName:"aiserver.v1.InAppAdService",methods:{ping:{name:"Ping",I:Keep,O:Keep,kind:n.MethodKind.Unary}}};
"#;
    let result = extract_from_sources(&[BUNDLE.to_string()], Strictness::Lenient).expect("extract");
    assert!(
        result
            .messages
            .iter()
            .any(|message| message.type_name == "agent.v1.Keep")
    );
    assert!(
        !result
            .messages
            .iter()
            .any(|message| message.type_name.contains("origin.v1"))
    );
    assert!(result.files.contains_key("agent_v1.proto"));
    assert!(result.files.contains_key("aiserver_v1.proto"));
    assert!(!result.files.keys().any(|name| name.contains("origin")));
}

impl super::BundleScan {
    fn source_end(&self) -> u32 {
        self.messages
            .iter()
            .map(|message| message.pos)
            .chain(self.services.iter().map(|service| service.pos))
            .max()
            .unwrap_or(0)
    }
}
