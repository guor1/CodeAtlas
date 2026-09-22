//! System prompts and response schemas for the L2 narrative layer.
//!
//! Every prompt enforces the same contract: answer only from the evidence given,
//! mark anything uncertain as `UNKNOWN` rather than inferring it, and return JSON
//! that names the evidence each claim rests on. This is what keeps the generated
//! documentation auditable instead of merely plausible.

use serde::Deserialize;

/// Shared preamble. Stated once, referenced by every task.
const GROUND_RULES: &str = r#"你是一位资深的代码考古学家，正在为一个缺乏文档的存量老项目重建业务领域知识。

严格遵守以下规则：
1. **只依据给定证据作答**。证据包括代码注释、枚举与常量定义、表结构、接口签名、调用链路、git 提交记录。不要依赖你对同类业务的一般印象去填补空白。
2. **不确定就写 UNKNOWN**，不要猜。一条标注 UNKNOWN 的条目比一条看起来合理但错误的描述有用得多——读者会去查代码，而错误描述会让他们跳过验证。
3. **区分「代码这样写」和「业务这样要求」**。只有注释或提交信息明确说明意图时，才可以描述业务意图；否则只描述代码行为。
4. **保留原始中文术语**。项目里的「特价」「满减」「整件购」「控销」等词是团队的通用语，不要翻译或改写成同义词。
5. 输出中文。除标识符、表名、字段名外不要夹杂英文。
6. 只输出 JSON，不要加 Markdown 代码围栏，不要有任何解释性前言或后记。"#;

/// Glossary extraction: business vocabulary and the code it maps to.
pub fn glossary_system() -> String {
    format!(
        r#"{GROUND_RULES}

本次任务：从证据中提取**业务术语表**。

只收录满足以下条件的术语：
- 是业务概念（活动类型、状态、角色、规则名称），而非技术概念（DTO、拦截器、连接池）
- 在证据中有明确定义或足以推断含义的上下文

输出 JSON：
{{
  "terms": [
    {{
      "term": "术语原文，保留项目里的写法",
      "definition": "一到两句定义。只写证据支持的内容",
      "aliases": ["同一概念在代码或注释里的其它叫法"],
      "code_refs": ["对应的标识符、枚举值、表名或字段名"],
      "confidence": "high | medium | low"
    }}
  ]
}}

宁缺毋滥：5 条准确的术语胜过 20 条含糊的。"#
    )
}

/// Domain dossier: what this area does, how state moves, where the traps are.
pub fn dossier_system() -> String {
    format!(
        r#"{GROUND_RULES}

本次任务：为一个业务领域写**领域档案**，读者是刚接手这块代码的工程师。

输出 JSON：
{{
  "responsibility": "这个领域负责什么。2-4 句，说清它在整个系统里的位置",
  "key_concepts": [
    {{"name": "核心概念名", "description": "它是什么、为什么存在"}}
  ],
  "lifecycle": {{
    "description": "主要业务对象的生命周期概述，没有足够证据时填 UNKNOWN",
    "states": [
      {{"state": "状态名", "code_value": "对应的枚举值或常量值", "meaning": "业务含义", "transitions_to": ["可以流转到的状态"]}}
    ]
  }},
  "rules": [
    {{"rule": "一条业务规则", "evidence": "支持这条规则的具体注释、常量或代码位置", "confidence": "high | medium | low"}}
  ],
  "landmines": [
    {{"issue": "接手这块代码容易踩的坑", "why": "为什么会踩", "evidence": "依据"}}
  ],
  "open_questions": ["证据不足、需要问业务方或老同事才能确认的问题"]
}}

关于各字段的要求：
- `lifecycle.states` 优先依据枚举与常量里的状态定义和注释；流转关系只在代码或注释明确体现时才写。
- `rules` 只写业务规则（谁在什么条件下能做什么、金额与库存怎么算、互斥关系），不要写代码结构说明。
- `landmines` 是这份文档最有价值的部分。重点关注：命名与实际行为不一致、同名概念在不同模块含义不同、已废弃但仍在运行的路径、注释里明确写了「这个没用」「有问题」的地方、改动最频繁的文件透露出的反复踩坑点。
- `open_questions` 不要客套，直接列出真正拦路的疑问。"#
    )
}

/// Capability narrative for a single entrypoint.
pub fn capability_system() -> String {
    format!(
        r#"{GROUND_RULES}

本次任务：说明**一个入口**做了什么，读者需要判断能不能调用它、调用后会发生什么。

输出 JSON：
{{
  "summary": "一句话说清这个入口的作用",
  "inputs": [{{"name": "参数名", "meaning": "业务含义", "required": true}}],
  "behavior": "按调用链路描述实际执行过程，点明关键分支",
  "side_effects": ["写了哪些表、发了哪些消息、改了哪些状态"],
  "rules": ["调用时必须满足的前置条件或业务约束"],
  "caveats": ["调用者容易误解的地方"]
}}"#
    )
}

// ------------------------------------------------------------------ schemas

#[derive(Debug, Deserialize)]
pub struct GlossaryResponse {
    #[serde(default)]
    pub terms: Vec<GlossaryTerm>,
}

#[derive(Debug, Deserialize)]
pub struct GlossaryTerm {
    pub term: String,
    #[serde(default)]
    pub definition: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub code_refs: Vec<String>,
    #[serde(default)]
    pub confidence: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DossierResponse {
    #[serde(default)]
    pub responsibility: String,
    #[serde(default)]
    pub key_concepts: Vec<Concept>,
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub landmines: Vec<Landmine>,
    #[serde(default)]
    pub open_questions: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Concept {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize)]
pub struct Lifecycle {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub states: Vec<State>,
}

#[derive(Debug, Deserialize)]
pub struct State {
    pub state: String,
    #[serde(default)]
    pub code_value: Option<String>,
    #[serde(default)]
    pub meaning: String,
    #[serde(default)]
    pub transitions_to: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Rule {
    pub rule: String,
    #[serde(default)]
    pub evidence: Option<String>,
    #[serde(default)]
    pub confidence: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Landmine {
    pub issue: String,
    #[serde(default)]
    pub why: String,
    #[serde(default)]
    pub evidence: Option<String>,
}

/// Capability narrative for a single entrypoint, mirroring `capability_system`.
#[derive(Debug, Deserialize)]
pub struct CapabilityResponse {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub inputs: Vec<CapabilityInput>,
    #[serde(default)]
    pub behavior: String,
    #[serde(default)]
    pub side_effects: Vec<String>,
    #[serde(default)]
    pub rules: Vec<String>,
    #[serde(default)]
    pub caveats: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CapabilityInput {
    pub name: String,
    #[serde(default)]
    pub meaning: String,
    #[serde(default)]
    pub required: bool,
}

/// Parse a model response into `T`, tolerating the wrappers models add.
///
/// Even with an explicit instruction not to, responses sometimes arrive fenced
/// or with a leading sentence. Recovering from that is cheaper than burning a
/// retry, and a retry would not reliably fix it anyway.
pub fn parse_json<T: for<'de> Deserialize<'de>>(raw: &str) -> anyhow::Result<T> {
    let text = strip_fence(raw.trim());
    if let Ok(v) = serde_json::from_str::<T>(text) {
        return Ok(v);
    }
    // Fall back to the outermost balanced object in the response. A truncated
    // response has no balanced object at all, so name that cause before the
    // generic "no JSON found", which sends the reader looking in the wrong place.
    let slice = outermost_object(text).ok_or_else(|| {
        if looks_truncated(text) {
            anyhow::anyhow!(
                "响应不完整（{} 字符），很可能被 max_tokens 截断——提高 max_tokens 或缩小任务粒度",
                text.chars().count()
            )
        } else {
            anyhow::anyhow!("响应中找不到 JSON 对象")
        }
    })?;
    serde_json::from_str::<T>(slice).map_err(|e| {
        anyhow::anyhow!(
            "解析 JSON 失败：{e}（响应 {} 字符，{}）",
            text.chars().count(),
            // Echoing the payload buries the diagnosis in thousands of characters
            // of the model's own output; name the likely cause instead.
            if looks_truncated(text) {
                "末尾不完整，很可能是响应被 max_tokens 截断——提高 max_tokens 或缩小任务粒度"
            } else {
                "格式不符合预期"
            }
        )
    })
}

/// True when a response ends mid-structure, the signature of a token-limit cut.
fn looks_truncated(text: &str) -> bool {
    let trimmed = strip_fence(text).trim_end();
    !trimmed.ends_with('}') && !trimmed.ends_with(']')
}

fn strip_fence(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else { return t };
    // Drop an optional language tag on the opening fence.
    let rest = match rest.find('\n') {
        Some(i) => &rest[i + 1..],
        None => rest,
    };
    rest.rsplit_once("```").map(|(body, _)| body.trim()).unwrap_or(rest.trim())
}

/// The outermost `{...}` span, respecting strings and escapes.
fn outermost_object(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_string {
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct Probe {
        a: String,
    }

    #[test]
    fn parses_bare_json() {
        let p: Probe = parse_json(r#"{"a":"x"}"#).unwrap();
        assert_eq!(p.a, "x");
    }

    #[test]
    fn strips_code_fences() {
        let p: Probe = parse_json("```json\n{\"a\":\"特价\"}\n```").unwrap();
        assert_eq!(p.a, "特价");
        let p: Probe = parse_json("```\n{\"a\":\"y\"}\n```").unwrap();
        assert_eq!(p.a, "y");
    }

    #[test]
    fn recovers_from_leading_prose() {
        let p: Probe = parse_json("这是结果：\n{\"a\":\"z\"}\n希望有帮助").unwrap();
        assert_eq!(p.a, "z");
    }

    #[test]
    fn braces_inside_strings_do_not_confuse_the_scanner() {
        let p: Probe = parse_json(r#"noise {"a":"值里有 } 和 \" 引号"} tail"#).unwrap();
        assert_eq!(p.a, "值里有 } 和 \" 引号");
    }

    #[test]
    fn nested_objects_take_the_outermost_span() {
        #[derive(Debug, Deserialize)]
        struct Outer {
            inner: Probe,
        }
        let o: Outer = parse_json(r#"{"inner":{"a":"deep"}}"#).unwrap();
        assert_eq!(o.inner.a, "deep");
    }

    #[test]
    fn missing_json_is_an_error() {
        assert!(parse_json::<Probe>("完全没有 JSON").is_err());
    }

    #[test]
    fn truncation_is_named_in_the_error() {
        // A response cut off mid-array, as a token limit produces.
        let cut = r#"{"terms":[{"term":"捡漏专区","definition":"清库存活动"#;
        let err = parse_json::<Probe>(cut).unwrap_err().to_string();
        assert!(err.contains("max_tokens"), "got: {err}");
        // The model's own output must not be echoed back into the error.
        assert!(!err.contains("捡漏专区"), "payload leaked: {err}");
    }

    #[test]
    fn malformed_but_complete_json_is_not_blamed_on_truncation() {
        let err = parse_json::<Probe>(r#"{"wrong_field": 1}"#).unwrap_err().to_string();
        assert!(err.contains("格式不符合预期"), "got: {err}");
    }

    #[test]
    fn truncation_detection_sees_through_fences() {
        assert!(looks_truncated("```json\n{\"a\":\"unfinished"));
        assert!(!looks_truncated("```json\n{\"a\":\"done\"}\n```"));
    }

    #[test]
    fn dossier_tolerates_missing_optional_fields() {
        let d: DossierResponse = parse_json(r#"{"responsibility":"管优惠券"}"#).unwrap();
        assert_eq!(d.responsibility, "管优惠券");
        assert!(d.rules.is_empty());
        assert!(d.lifecycle.is_none());
    }

    #[test]
    fn capability_tolerates_missing_optional_fields() {
        let c: CapabilityResponse = parse_json(r#"{"summary":"查优惠券"}"#).unwrap();
        assert_eq!(c.summary, "查优惠券");
        assert!(c.inputs.is_empty());
        assert!(c.side_effects.is_empty());
        assert!(c.caveats.is_empty());
        // Optional sub-fields default too.
        let c: CapabilityResponse =
            parse_json(r#"{"summary":"s","inputs":[{"name":"id"}]}"#).unwrap();
        assert_eq!(c.inputs[0].meaning, "");
        assert!(!c.inputs[0].required);
    }

    #[test]
    fn prompts_carry_the_ground_rules() {
        for p in [glossary_system(), dossier_system(), capability_system()] {
            assert!(p.contains("UNKNOWN"));
            assert!(p.contains("只依据给定证据作答"));
            assert!(p.contains("只输出 JSON"));
        }
    }
}
