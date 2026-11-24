//! Text inference through the selected module. Tool execution stays in the core.
use crate::{plugins::Host,settings::{Profile,Settings},tools::Tools};
use serde_json::{Value,json};
use std::time::Duration;

fn configured<'a>(s:&'a Settings,id:&str)->&'a str {
    match id{
        "openai"=>&s.openai_key,
        "gemini"=>if !s.gemini_key.is_empty(){&s.gemini_key}else{s.gemini_keys.first().map(String::as_str).unwrap_or("")},
        "grok"=>&s.grok_key,
        _=>""
    }
}

pub const MEMORY_PROVIDERS: [&str; 3] = ["chatgpt", "gemini", "openai"];
pub const GEMINI_TEXT_MODEL: &str = "gemini-3.8-flash";

pub fn memory_available(host: &Host, settings: &Settings, provider: &str) -> bool {
    let configured = if provider == "chatgpt" {
        crate::chatgpt::signed_in(&settings.chatgpt)
    } else {
        !configured(settings, provider).is_empty()
    };
    configured && host.provider(provider).is_ok()
}

pub fn memory_candidates(host: &Host, settings: &Settings) -> Vec<&'static str> {
    if settings.memory.off || settings.memory.provider == "none" { return vec![]; }
    let candidates: Vec<_> = MEMORY_PROVIDERS.into_iter()
        .filter(|provider| memory_available(host, settings, provider)).collect();
    if let Some(i) = candidates.iter().position(|provider| *provider == settings.memory.provider) {
        let selected = candidates[i];
        return vec![selected];
    }
    candidates
}

pub fn memory_model<'a>(settings: &'a Settings, provider: &str) -> &'a str {
    let selected = &settings.memory.model;
    let use_selected = settings.memory.provider.is_empty() || settings.memory.provider == provider;
    if provider == "gemini" {
        return if use_selected && selected.starts_with("gemini-") { selected } else { GEMINI_TEXT_MODEL };
    }
    if use_selected && crate::settings::BACKEND_MODELS.contains(&selected.as_str()) {
        selected
    } else {
        crate::settings::BACKEND_MODELS[0]
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Turn {
    pub role: &'static str,
    pub content: String,
}

pub struct AskReply {
    pub text: String,
    pub close_dialog: bool,
}

pub async fn ask(host:&Host,s:&Settings,p:&Profile,instructions:&str,text:&str,tools:&Tools,defs:Vec<Value>)->Result<String,String>{
    ask_with_history(host, s, p, instructions, &[], text, tools, defs).await
}

pub async fn ask_with_pool(host:&Host,s:&Settings,p:&Profile,instructions:&str,text:&str,tools:&Tools,defs:Vec<Value>,gemini_pool:Option<&crate::gemini_pool::GeminiPool>)->Result<String,String>{
    ask_with_history_detail_pool(host, s, p, instructions, &[], text, tools, defs, gemini_pool)
        .await
        .map(|reply| reply.text)
}

pub async fn ask_with_history(host:&Host,s:&Settings,p:&Profile,instructions:&str,history:&[Turn],text:&str,tools:&Tools,defs:Vec<Value>)->Result<String,String>{
    ask_with_history_detail(host, s, p, instructions, history, text, tools, defs)
        .await
        .map(|reply| reply.text)
}

pub async fn ask_with_history_detail(host:&Host,s:&Settings,p:&Profile,instructions:&str,history:&[Turn],text:&str,tools:&Tools,defs:Vec<Value>)->Result<AskReply,String>{
    ask_with_history_detail_pool(host, s, p, instructions, history, text, tools, defs, None).await
}

pub async fn ask_with_history_detail_pool(host:&Host,s:&Settings,p:&Profile,instructions:&str,history:&[Turn],text:&str,tools:&Tools,defs:Vec<Value>,gemini_pool:Option<&crate::gemini_pool::GeminiPool>)->Result<AskReply,String>{
    let explicit = p.effective_llm_engine();
    if !explicit.is_empty() && !matches!(explicit, "openai" | "live" | "chatgpt" | "gemini" | "grok") {
        return ask_plugin(host, explicit, p.effective_llm_model(), instructions, history, text, tools, &defs).await;
    }
    let id = if !explicit.is_empty() {
        let id = if explicit == "live" { "openai" } else { explicit };
        if id == "chatgpt" {
            if !crate::chatgpt::signed_in(&s.chatgpt) {
                return Err("Войдите в ChatGPT в «Подключениях»".into());
            }
            let auth = host.provider("chatgpt")?.request("auth.token", Value::Null, Duration::from_secs(20)).await?;
            return crate::chatgpt::ask_codex_with_history_detail(host, auth, p, instructions, history, text, Some(tools), defs)
                .await
                .map(|r| AskReply { text: r.text, close_dialog: r.close_dialog });
        }
        if configured(s, id).is_empty() {
            return Err(match id {
                "openai" => "Добавьте ключ OpenAI в «Подключениях»",
                "gemini" => "Добавьте ключ Gemini в «Подключениях»",
                "grok" => "Добавьте ключ Grok в «Подключениях»",
                _ => "Добавьте API-ключ в «Подключениях»",
            }.into());
        }
        id
    } else {
        let preferred=if ["openai","live",""].contains(&p.engine.as_str()){"openai"}else{p.engine.as_str()};
        if preferred=="chatgpt" && crate::chatgpt::signed_in(&s.chatgpt){
            let auth=host.provider("chatgpt")?.request("auth.token",Value::Null,Duration::from_secs(20)).await?;
            return crate::chatgpt::ask_codex_with_history_detail(host,auth,p,instructions,history,text,Some(tools),defs)
                .await
                .map(|r| AskReply { text: r.text, close_dialog: r.close_dialog });
        }
        let id=if !configured(s,preferred).is_empty(){preferred}else{["openai","gemini","grok"].into_iter().find(|id|!configured(s,id).is_empty()).unwrap_or("chatgpt")};
        if id=="chatgpt" {
            if !crate::chatgpt::signed_in(&s.chatgpt){return Err("Добавьте API-ключ или войдите в ChatGPT в «Подключениях»".into());}
            let auth=host.provider(id)?.request("auth.token",Value::Null,Duration::from_secs(20)).await?;
            return crate::chatgpt::ask_codex_with_history_detail(host,auth,p,instructions,history,text,Some(tools),defs)
                .await
                .map(|r| AskReply { text: r.text, close_dialog: r.close_dialog });
        }
        id
    };
    let provider=host.provider(id)?;let key=configured(s,id);
    let requested = p.effective_llm_model();
    let model=match id {
        "gemini"=>if requested.starts_with("gemini-"){requested}else{GEMINI_TEXT_MODEL},
        "grok"=>if requested.starts_with("grok-"){requested}else{"grok-4-1-fast-non-reasoning"},
        _=>if requested.is_empty(){crate::settings::BACKEND_MODELS[0]}else{requested},
    };
    let mut input:Vec<Value>=history.iter().map(|t|json!({"role":t.role,"content":t.content})).collect();
    input.push(json!({"role":"user","content":text}));
    let mut contents:Vec<Value>=history.iter().map(|t|json!({"role":if t.role=="assistant"{"model"}else{"user"},"parts":[{"text":t.content}]})).collect();
    contents.push(json!({"role":"user","parts":[{"text":text}]}));
    let mut messages=vec![json!({"role":"system","content":instructions})];
    for turn in history {
        messages.push(json!({"role":turn.role,"content":turn.content}));
    }
    messages.push(json!({"role":"user","content":text}));
    let mut music_turn = crate::music::MusicTurnTracker::default();
    for _ in 0..5 {
        let body=match id {
            "gemini"=>{
                let declarations=crate::gemini::declarations(&defs);
                let mut body=json!({"contents":contents,"systemInstruction":{"parts":[{"text":instructions}]}});
                let mut tools=vec![];if !declarations.is_empty(){tools.push(json!({"functionDeclarations":declarations}));}if p.web_search{tools.push(json!({"googleSearch":{}}));}if !tools.is_empty(){body["tools"]=tools.into();}body
            },
            "grok"=>{let tools:Vec<_>=defs.iter().filter(|d|d["type"]=="function").map(|d|json!({"type":"function","function":{"name":d["name"],"description":d["description"],"parameters":d["parameters"]}})).collect();let mut body=json!({"model":model,"messages":messages,"stream":false});if !tools.is_empty(){body["tools"]=tools.into();}body},
            _=>{let mut tools=defs.clone();if p.web_search{tools.push(json!({"type":"web_search"}));}json!({"model":model,"instructions":instructions,"input":input,"tools":tools,"reasoning":{"effort":crate::chatgpt::effort(model,&p.effort)},"store":false,"stream":false})},
        };
        let response = if id == "gemini" {
            let tz = jiff::tz::TimeZone::get(&s.timezone).unwrap_or(jiff::tz::TimeZone::UTC);
            let all_keys = crate::gemini_pool::effective_keys(&s.gemini_keys, &s.gemini_key);
            if let Some(pool) = gemini_pool {
                let cands = pool.candidates(&all_keys, &s.gemini_key, &[model.to_string()], crate::gemini_pool::now_unix());
                if cands.is_empty() {
                    let f = pool.all_exhausted_failure(&all_keys, &[model.to_string()], crate::gemini_pool::now_unix(), &tz);
                    log::warn!("Gemini (текст): {} | Что делать: {}", f.detail, f.action);
                    return Err(f.detail);
                }
                let mut reply = None;
                for (i, cand) in cands.iter().enumerate() {
                    match provider.request(id, json!({"op":"text","key":&cand.key,"model":model,"body":body}), Duration::from_secs(60)).await {
                        Ok(val) => {
                            pool.record_success(&cand.key, model);
                            reply = Some(val);
                            break;
                        }
                        Err(err) => {
                            let failure = crate::voice_error::classify("gemini", crate::voice_error::Stage::Connect, &err, None, false);
                            if let Some(ev) = pool.record_failure(&cand.key, cand.key_index, model, &failure, crate::gemini_pool::now_unix(), &tz) {
                                if let Some(next) = cands.get(i + 1) {
                                    log::warn!("Gemini (текст): {}. Переключаюсь на ключ #{} ({}).", ev.summary, next.key_index + 1, next.tail);
                                    continue;
                                }
                                let final_f = pool.all_exhausted_failure(&all_keys, &[model.to_string()], crate::gemini_pool::now_unix(), &tz);
                                log::warn!("Gemini (текст): {} | Что делать: {}", final_f.detail, final_f.action);
                                return Err(final_f.detail);
                            }
                            return Err(err);
                        }
                    }
                }
                match reply {
                    Some(v) => v,
                    None => return Err("Все ключи Gemini в лимите".into()),
                }
            } else {
                provider.request(id, json!({"op":"text","key":key,"model":model,"body":body}), Duration::from_secs(60)).await?
            }
        } else {
            provider.request(id, json!({"op":"text","key":key,"model":model,"body":body}), Duration::from_secs(60)).await?
        };
        let (answer,calls)=decode(id,&response)?;
        if response["candidates"][0].get("groundingMetadata").is_some()
            || response["output"].as_array().is_some_and(|items| items.iter().any(|item| item["type"] == "web_search_call"))
        {
            music_turn.note_other();
        }
        if calls.is_empty(){
            return if answer.trim().is_empty(){
                Err("Модель ничего не ответила".into())
            } else {
                let close_dialog = music_turn.should_close(&answer);
                Ok(AskReply { text: answer, close_dialog })
            };
        }
        match id {
            "gemini"=>contents.push(response["candidates"][0]["content"].clone()),
            "grok"=>messages.push(response["choices"][0]["message"].clone()),
            _=>input.extend(response["output"].as_array().cloned().unwrap_or_default()),
        }
        let mut gemini_results=vec![];
        for call in calls {
            let result = run_tool(tools, &defs, &call).await?;
            music_turn.observe_tool(&call.name, &call.arguments, &result);
            match id {
                "gemini"=>{let mut value=json!({"name":call.name,"response":{"result":result}});if !call.id.is_empty(){value["id"]=call.id.into();}gemini_results.push(json!({"functionResponse":value}));},
                "grok"=>messages.push(json!({"role":"tool","tool_call_id":call.id,"content":result})),
                _=>input.push(json!({"type":"function_call_output","call_id":call.id,"output":result})),
            }
        }
        if !gemini_results.is_empty(){contents.push(json!({"role":"user","parts":gemini_results}));}
    }
    Err("Модель не закончила за 5 шагов".into())
}
async fn run_tool(tools: &Tools, defs: &[Value], call: &Call) -> Result<String, String> {
    if call.name.is_empty() || call.name.len() > 100 || call.arguments.len() > 256 * 1024 {
        return Err("Неверный вызов инструмента".into());
    }
    if !defs.iter().any(|def| def["name"] == call.name) {
        return Err("Модель вызвала необъявленный инструмент".into());
    }
    let limit = if call.name == "play_music" { 35 } else { 15 };
    Ok(match tokio::time::timeout(Duration::from_secs(limit), tools.call(&call.name, &call.arguments)).await {
        Ok(result) => result,
        Err(_) => {
            if call.name == "play_music" {
                tools.cancel_loading_music().await;
            }
            format!("Инструмент не ответил за {limit} с")
        }
    })
}

async fn ask_plugin(host: &Host, id: &str, model: &str, instructions: &str, history: &[Turn], text: &str, tools: &Tools, defs: &[Value]) -> Result<AskReply, String> {
    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(json!({"role":"system","content":instructions}));
    for turn in history {
        messages.push(json!({"role":turn.role,"content":turn.content}));
    }
    messages.push(json!({"role":"user","content":text}));
    let schemas: Vec<_> = defs.iter().filter(|def| def["type"] == "function").map(|def| {
        json!({"type":"function","function":{"name":def["name"],"description":def["description"],"parameters":def["parameters"]}})
    }).collect();
    let mut music_turn = crate::music::MusicTurnTracker::default();
    for _ in 0..5 {
        let mut payload = json!({"messages":messages,"tools":schemas});
        if !model.is_empty() {
            payload["model"] = json!(model);
        }
        let response = host.plugin_chat(id, payload).await?;
        let answer = response.as_str().or_else(|| response["text"].as_str()).unwrap_or_default().trim();
        let Some(calls) = response.get("tool_calls").and_then(Value::as_array).filter(|calls| !calls.is_empty()) else {
            if answer.is_empty() || answer.len() > 32 * 1024 {
                return Err("Текстовый плагин вернул пустой или слишком длинный ответ".into());
            }
            return Ok(AskReply {
                text: answer.to_owned(),
                close_dialog: music_turn.should_close(answer),
            });
        };
        if calls.len() > 16 { return Err("Плагин вернул слишком много вызовов инструментов".into()); }
        let mut parsed = Vec::with_capacity(calls.len());
        let mut ids = std::collections::HashSet::new();
        for call in calls {
            let field = |key| call[key].as_str().ok_or("Неверный вызов инструмента от плагина");
            let id = field("id")?;
            let name = field("name")?;
            let arguments = field("arguments")?;
            if id.is_empty() || id.len() > 128 || !ids.insert(id)
                || arguments.len() > 32 * 1024 || !serde_json::from_str::<Value>(arguments).is_ok_and(|v| v.is_object())
                || !schemas.iter().any(|schema| schema["function"]["name"] == name)
            {
                return Err("Плагин вернул неверный или необъявленный инструмент".into());
            }
            parsed.push(Call { id: id.into(), name: name.into(), arguments: arguments.into() });
        }
        let tool_calls: Vec<_> = parsed.iter().map(|call| json!({"id":call.id,"type":"function","function":{"name":call.name,"arguments":call.arguments}})).collect();
        messages.push(json!({"role":"assistant","content":answer,"tool_calls":tool_calls}));
        for call in parsed {
            let output = run_tool(tools, defs, &call).await?;
            music_turn.observe_tool(&call.name, &call.arguments, &output);
            messages.push(json!({"role":"tool","tool_call_id":call.id,"content":output}));
        }
    }
    Err("Текстовый плагин не закончил за 5 шагов".into())
}

struct Call {name:String,arguments:String,id:String}
fn decode(id:&str,response:&Value)->Result<(String,Vec<Call>),String>{
    let mut answer=String::new();let mut calls=vec![];
    let field=|v:&Value,k:&str|v[k].as_str().unwrap_or("").to_string();
    match id {
        "gemini"=>{
            let parts=response["candidates"][0]["content"]["parts"].as_array().ok_or("Gemini не вернул ответ")?;
            for part in parts {if let Some(text)=part["text"].as_str().filter(|_|part["thought"]!=true){answer.push_str(text);}if part["functionCall"].is_object(){let c=&part["functionCall"];calls.push(Call{name:field(c,"name"),arguments:c["args"].to_string(),id:field(c,"id")});}}
        },
        "grok"=>{let message=&response["choices"][0]["message"];answer=field(message,"content");for call in message["tool_calls"].as_array().into_iter().flatten(){calls.push(Call{name:field(&call["function"],"name"),arguments:field(&call["function"],"arguments"),id:field(call,"id")});}},
        _=>{for item in response["output"].as_array().ok_or("OpenAI не вернул output")?{match item["type"].as_str(){Some("function_call")=>calls.push(Call{name:field(item,"name"),arguments:field(item,"arguments"),id:field(item,"call_id")}),Some("message")=>for part in item["content"].as_array().into_iter().flatten(){if part["type"]=="output_text"{answer.push_str(&field(part,"text"));}},_=>{}}}},
    }
    if calls.len()>32{return Err("Модель вернула слишком много инструментов".into());}Ok((answer,calls))
}
#[cfg(test)]mod tests{use super::*;#[test]fn decodes_provider_tool_calls_without_speaking_thoughts(){
    let (answer,calls)=decode("gemini",&json!({"candidates":[{"content":{"parts":[{"text":"private","thought":true},{"text":"Ответ"},{"functionCall":{"name":"test","args":{"a":1},"id":"x"}}]}}]})).unwrap();assert_eq!(answer,"Ответ");assert_eq!(calls[0].name,"test");assert_eq!(calls[0].arguments,"{\"a\":1}");
    let (_,calls)=decode("openai",&json!({"output":[{"type":"function_call","name":"x","arguments":"{}","call_id":"id"}]})).unwrap();assert_eq!(calls[0].id,"id");
}

    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn selected_lua_provider_can_use_a_declared_tool() {
        let dir = std::env::temp_dir().join(format!("jarvis-llm-{}", crate::settings::rand_hex(6)));
        let plugin = dir.join("plugins/example-llm");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("plugin.json"), json!({
            "id":"example-llm","name":"Example","version":"1.0.0","kind":"lua","category":"assistant",
            "main":"main.lua","api":1,"provides":["llm"],"permissions":["tools"]
        }).to_string()).unwrap();
        std::fs::write(plugin.join("main.lua"), r#"return {
            chat = function(request)
                local last = request.messages[#request.messages]
                if last.role == 'tool' then return last.content end
                assert(request.messages[1].content == 'system text')
                assert(request.tools[1]["function"].name == 'probe')
                local name = last.content == 'denied' and 'undeclared' or 'probe'
                return { tool_calls = {{ id = 'one', name = name, arguments = '{"text":"works"}' }} }
            end,
            tools = {{name = 'probe', description = 'Test', parameters = {type = 'object'},
                handler = function(args) return args.text end}}
        }"#).unwrap();
        let host = Host::new(dir.clone(), crate::plugins::Hooks::default());
        host.load().await;
        assert_eq!(host.text_plugins()[0]["id"], "example-llm");
        let store = crate::settings::Store::open(dir.join("settings.json")).unwrap();
        let tools = Tools::new(host.clone(), store);
        let profile = Profile { llm_plugin: "example-llm".into(), ..Default::default() };
        let reply = ask(&host, &Settings::default(), &profile, "system text", "test", &tools, host.tools()).await;
        assert_eq!(reply.as_deref(), Ok("works"));
        let denied = ask(&host, &Settings::default(), &profile, "system text", "denied", &tools, host.tools()).await;
        assert!(denied.unwrap_err().contains("необъявленный"));
        host.set_enabled("example-llm", false).await.unwrap();
        assert!(host.text_plugins().is_empty());
        assert!(ask(&host, &Settings::default(), &profile, "system text", "test", &tools, vec![]).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(feature = "lua")]
    #[tokio::test]
    async fn lua_openrouter_example_forwards_model_and_history_with_tools() {
        let dir = std::env::temp_dir().join(format!("jarvis-openrouter-{}", crate::settings::rand_hex(6)));
        let plugin = dir.join("plugins/lua-openrouter");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(
            plugin.join("plugin.json"),
            include_str!("../../../plugins/examples/lua-openrouter/plugin.json"),
        ).unwrap();
        let src = format!(
            r#"
            jarvis.settings = {{ api_key = 'sk-or-test', model = 'default/model' }}
            jarvis.fetch = function(url, opts)
                local req = jarvis.json.decode(opts.body)
                assert(url == 'https://openrouter.ai/api/v1/chat/completions')
                assert(req.model == 'qwen/qwen3-235b')
                assert(req.messages[2].role == 'user' and req.messages[2].content == 'prev q')
                assert(req.messages[3].role == 'assistant' and req.messages[3].content == 'prev a')
                local last = req.messages[#req.messages]
                if last.role == 'tool' then
                    return {{ ok = true, status = 200, body = jarvis.json.encode({{ choices = {{ {{ message = {{ content = 'got:' .. last.content }} }} }} }}) }}
                end
                return {{ ok = true, status = 200, body = jarvis.json.encode({{ choices = {{ {{ message = {{ content = '', tool_calls = {{ {{ id = 'c1', ["function"] = {{ name = 'probe', arguments = '{{"x":"ok"}}' }} }} }} }} }} }} }}) }}
            end
            local mod = (function()
            {}
            end)()
            mod.tools = {{ {{ name = 'probe', description = 'Test', parameters = {{type = 'object'}}, handler = function(a) return a.x end }} }}
            return mod
            "#,
            include_str!("../../../plugins/examples/lua-openrouter/main.lua")
        );
        // Add "tools" permission just for the mocked test tool in the same plugin.
        let mut manifest: Value = serde_json::from_str(include_str!("../../../plugins/examples/lua-openrouter/plugin.json")).unwrap();
        manifest["permissions"] = json!(["network", "tools"]);
        std::fs::write(plugin.join("plugin.json"), manifest.to_string()).unwrap();
        std::fs::write(plugin.join("main.lua"), src).unwrap();

        let host = Host::new(dir.clone(), crate::plugins::Hooks::default());
        host.load().await;
        let store = crate::settings::Store::open(dir.join("settings.json")).unwrap();
        let tools = Tools::new(host.clone(), store);
        let profile = Profile {
            llm_engine: "lua-openrouter".into(),
            llm_model: "qwen/qwen3-235b".into(),
            ..Default::default()
        };
        let history = [
            Turn { role: "user", content: "prev q".into() },
            Turn { role: "assistant", content: "prev a".into() },
        ];
        let reply = ask_with_history(&host, &Settings::default(), &profile, "sys", &history, "now", &tools, host.tools()).await;
        assert_eq!(reply.as_deref(), Ok("got:ok"));
        host.stop_all().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
