use crate::{plugins::{Host,Runtime},session::{Dialog,Session,queue_mic,take_mic},journal::Journal,settings::{Settings,Profile},tools::Tools};
use std::{sync::{Arc,Mutex,atomic::{AtomicBool,Ordering},mpsc::{Receiver,SyncSender,TrySendError,sync_channel}},time::Duration};
use serde_json::{Value,json};
use base64::{Engine as _,engine::general_purpose::STANDARD as B64};

pub struct Voice {
    id:String,dialog:Dialog,rt:tokio::runtime::Handle,pending:Mutex<Vec<i16>>,
    output:Mutex<Option<SyncSender<Vec<i16>>>>,audio:Mutex<Option<Receiver<Vec<i16>>>>,
    ready:Mutex<Option<Receiver<Result<(),String>>>>,commands:tokio::sync::mpsc::Sender<String>,
    cancelled:AtomicBool,close:tokio::sync::Notify,interrupted:AtomicBool,
}
impl Voice {
    pub fn open(rt:&tokio::runtime::Handle,host:Arc<Host>,tools:Option<Arc<Tools>>,journal:Arc<Journal>,settings:Settings,profile:Profile,instructions:String)->Arc<Self>{
        let id=match profile.engine.as_str(){""|"live"=>"openai","eleven_realtime"=>"elevenlabs",id=>id}.to_string();
        let (tx,rx)=sync_channel(128);let (ready,ready_rx)=sync_channel(1);let (commands,command_rx)=tokio::sync::mpsc::channel(16);
        let me=Arc::new(Self{id:id.clone(),dialog:Dialog::new(journal,&id,&profile.live_model),rt:rt.clone(),pending:Mutex::default(),output:Mutex::new(Some(tx)),audio:Mutex::new(Some(rx)),ready:Mutex::new(Some(ready_rx)),commands,cancelled:AtomicBool::new(false),close:tokio::sync::Notify::new(),interrupted:AtomicBool::new(false)});
        let run=me.clone();rt.spawn(async move{
            let result=tokio::select!{result=run.run(host,tools,settings,profile,instructions,ready.clone(),command_rx)=>result,_=run.close.notified()=>Ok(())};
            if let Err(error)=result {let _=ready.try_send(Err(error.clone()));run.dialog.finish_turn(&error);}
            run.output.lock().unwrap().take();run.cancelled.store(true,Ordering::SeqCst);
        });me
    }
    async fn run(&self,host:Arc<Host>,tools:Option<Arc<Tools>>,settings:Settings,p:Profile,instructions:String,ready:SyncSender<Result<(),String>>,mut commands:tokio::sync::mpsc::Receiver<String>)->Result<(),String>{
        let provider=host.provider(&self.id)?;
        let key=match self.id.as_str(){"openai"=>&settings.openai_key,"gemini"=>&settings.gemini_key,"grok"=>&settings.grok_key,"elevenlabs"=>&settings.elevenlabs_key,_=>return Err("Этот движок требует классический режим".into())};
        let prepare=async { match &tools { Some(tools)=>tools.prepare().await, None=>(String::new(), vec![]) } };
        let ((context,defs),mut socket)=if self.id=="elevenlabs"{
            let (context,defs)=prepare.await;let session=eleven_session(&p,&format!("{instructions}\n{context}"),&defs);
            let socket=Link::open(provider,key,session).await?;((context,defs),socket)
        }else{tokio::try_join!(async{Ok::<_,String>(prepare.await)},Link::open(provider,key,Value::Null))?};
        let model=if p.live_model.is_empty(){match self.id.as_str(){"gemini"=>"gemini-3.8-live","grok"=>"grok-voice-latest",_=>"gpt-live-1"}}else{&p.live_model};
        let voice=if p.voice.is_empty(){match self.id.as_str(){"gemini"=>"Kore","grok"=>"Ara",_=>"cedar"}}else{&p.voice};
        let start=match self.id.as_str(){
            "gemini"=>crate::gemini::live_setup(model, voice, &format!("{instructions}\n{context}"), &settings.language, &defs, tools.is_some() && p.web_search),
            "grok"=>json!({"type":"session.update","session":{"instructions":format!("{instructions}\n{context}"),"voice":voice,"turn_detection":{"type":"server_vad"},"audio":{"input":{"format":{"type":"audio/pcm","rate":24000}},"output":{"format":{"type":"audio/pcm","rate":24000}}},"tools":defs}}),
            _=>{let mut backend=defs.clone();if p.web_search{backend.push(json!({"type":"web_search"}));}json!({"type":"session.start","session":{"model":model,"instructions":instructions,"audio":{"format":{"type":"audio/pcm","rate":16000},"output":{"voice":voice}},"delegation":{"type":"responses","responses":{"model":if p.backend_model.is_empty(){crate::settings::BACKEND_MODELS[0]}else{&p.backend_model},"instructions":format!("{}\n{context}",crate::rules::BACKEND_RULES),"tools":backend,"tool_choice":"auto","reasoning":{"effort":crate::chatgpt::effort(&p.backend_model,&p.effort)}}}}})},
        };
        if self.id!="elevenlabs"{send(&mut socket,start).await?;}
        let gemini = self.id == "gemini";
        // Send the real-time stream every 40 ms. Drain up to 160 ms per tick
        // while catching up with speech captured during setup.
        const TICK_MS: u64 = 40;
        const SAMPLES_PER_TICK: usize = crate::audio::MIC_RATE as usize * TICK_MS as usize / 1000;
        const MAX_SAMPLES_PER_TICK: usize = SAMPLES_PER_TICK * 4;
        let mut started=false;let mut tasks=tokio::task::JoinSet::<(String,String,String,Value)>::new();let mut interval=tokio::time::interval(Duration::from_millis(TICK_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut handles=std::collections::HashMap::<String,tokio::task::AbortHandle>::new();let mut seen=std::collections::HashSet::new();
        let mut pending_calls=Vec::<Value>::new();
        let mut music_turn = crate::music::MusicTurnTracker::default();
        let mut music_turn_id = 0i64;
        let mut up=jarvis_dsp::audio::Upsampler::new(3);let mut phase=0;let mut speaking=false;
        loop {
            tokio::select!{
                biased;
                result=tasks.join_next(),if !tasks.is_empty()=>{
                    self.dialog.working(-1);
                    let (name,args,outcome,value)=match result.ok_or("Вызов инструмента завершился")? {Ok(item)=>item,Err(e) if e.is_cancelled()=>continue,Err(e)=>return Err(e.to_string())};
                    let id=value.pointer("/toolResponse/functionResponses/0/id").or_else(||value.pointer("/item/call_id")).and_then(Value::as_str).unwrap_or("");handles.remove(id);
                    let turn_id = self.dialog.utterance().0;
                    if turn_id != music_turn_id {
                        music_turn = crate::music::MusicTurnTracker::default();
                        music_turn_id = turn_id;
                    }
                    music_turn.observe_tool(&name, &args, &outcome);
                    if tasks.is_empty()
                        && music_turn.should_close(&outcome)
                        && !crate::music::wants_keep_talking(&self.dialog.utterance().1)
                    {
                        self.dialog.on_bot_note(&outcome);
                        self.dialog.mute();
                        self.dialog.request_close();
                        return Ok(());
                    }
                    send(&mut socket,value).await?;if self.id!="gemini" && (self.id!="elevenlabs" || tasks.is_empty()){send(&mut socket,json!({"type":"response.create"})).await?;}
                },
                Some(text)=commands.recv(),if started=>{
                    let value=match self.id.as_str(){"gemini"=>json!({"clientContent":{"turns":[{"role":"user","parts":[{"text":format!("Произнеси дословно: {text}")}]}],"turnComplete":true}}),"grok"|"elevenlabs"=>json!({"type":"conversation.item.create","item":{"type":"message","role":"user","content":[{"type":"input_text","text":format!("Произнеси дословно: {text}")}]}}),_=>json!({"type":"session.commentary.append","delegation_id":null,"content":text})};send(&mut socket,value).await?;
                    if matches!(self.id.as_str(),"grok"|"elevenlabs"){send(&mut socket,json!({"type":"response.create"})).await?;}
                },
                _=interval.tick(),if started && (!gemini || tools.is_some())=>{
                    let mut pcm=take_mic(&mut self.pending.lock().unwrap(),MAX_SAMPLES_PER_TICK);if pcm.is_empty(){pcm.resize(SAMPLES_PER_TICK,0);}
                    if matches!(self.id.as_str(),"grok"|"elevenlabs"){let input=up.run(&pcm);pcm=input.iter().skip(phase).step_by(2).copied().collect();phase=(phase+input.len())%2;}
                    let data=B64.encode(pcm.iter().flat_map(|v|v.to_le_bytes()).collect::<Vec<_>>());
                    let value=match self.id.as_str(){"gemini"=>json!({"realtimeInput":{"audio":{"mimeType":"audio/pcm;rate=16000","data":data}}}),"grok"|"elevenlabs"=>json!({"type":"input_audio_buffer.append","audio":data}),_=>json!({"type":"session.input_audio.append","audio":data})};send(&mut socket,value).await?;
                },
                message=socket.events.recv()=>{
                    let Some(message)=message else{return Ok(());};let mut message=message?;
                    if let Some(close) = message["close"].as_str() {
                        if !started {
                            let detail = close.replace(key,"***").chars().take(220).collect::<String>();
                            return Err(if detail.is_empty() { format!("{} закрыл голосовое соединение до начала разговора", self.id) } else { format!("{}: {detail}",self.id) });
                        }
                        return Ok(());
                    }
                    let event=message["event"].take();
                    for id in event.pointer("/toolCallCancellation/ids").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str){if let Some(handle)=handles.remove(id){handle.abort();}}
                    let kind=event["type"].as_str().unwrap_or("");
                    if event.get("setupComplete").is_some() || ["session.started","session.updated","session.created"].contains(&kind){if !started{started=true;let _=ready.try_send(Ok(()));}continue;}
                    if kind=="error" || event.get("error").is_some(){let message=event.pointer("/error/message").and_then(Value::as_str).unwrap_or("Провайдер отклонил голосовую сессию");return Err(message.replace(key,"***").chars().take(240).collect());}
                    if ["session.closed"].contains(&kind){return Ok(());}
                    if kind=="input_audio_buffer.speech_started" {self.dialog.heard_input(std::time::Instant::now());}
                    if kind=="input_audio_buffer.speech_started" || event.pointer("/serverContent/interrupted").and_then(Value::as_bool)==Some(true){self.dialog.mute();self.interrupted.store(true,Ordering::SeqCst);speaking=false;}
                    if kind=="session.input_transcript.delta" || kind=="conversation.item.input_audio_transcription.completed"{self.dialog.on_user(event[if kind.ends_with("completed"){"transcript"}else{"delta"}].as_str().unwrap_or(""));if kind.ends_with("completed"){self.dialog.unmute();}}
                    if ["session.output_transcript.delta","response.audio_transcript.delta","response.output_audio_transcript.delta"].contains(&kind){self.dialog.on_bot(event["delta"].as_str().unwrap_or(""));}
                    if ["session.output_audio.delta","response.audio.delta","response.output_audio.delta"].contains(&kind){self.audio(event["delta"].as_str().unwrap_or("")).await?;speaking=true;}
                    if let Some(content)=event.get("serverContent"){
                        if content.get("groundingMetadata").is_some(){
                            log::debug!("Gemini Live: получены результаты поиска");
                            let turn_id = self.dialog.utterance().0;
                            if turn_id != music_turn_id {
                                music_turn = crate::music::MusicTurnTracker::default();
                                music_turn_id = turn_id;
                            }
                            music_turn.note_other();
                        }
                        if let Some(text)=content.pointer("/inputTranscription/text").and_then(Value::as_str){self.dialog.on_user(text);}
                        if let Some(text)=content.pointer("/outputTranscription/text").and_then(Value::as_str){self.dialog.on_bot(text);}
                        for part in content.pointer("/modelTurn/parts").and_then(Value::as_array).into_iter().flatten(){if let Some(data)=part.pointer("/inlineData/data").and_then(Value::as_str){self.audio(data).await?;speaking=true;}}
                        if content["turnComplete"].as_bool()==Some(true){self.dialog.provider_turn_complete();}
                    }
                    if kind=="session.delegation.created"{self.dialog.working(1);}
                    if kind=="response.event"{let inner=event["event"]["type"].as_str().unwrap_or("");if ["response.completed","response.failed","response.cancelled","response.incomplete"].contains(&inner){self.dialog.working(-1);}}
                    let mut calls=vec![];
                    if kind=="response.event" && event["event"]["type"]=="response.output_item.done" && event["event"]["item"]["type"]=="function_call"{calls.push(event["event"]["item"].clone());}
                    if kind=="response.function_call_arguments.done"{
                        if self.id=="elevenlabs"{if pending_calls.len()>=32{return Err("Слишком много вызовов инструментов".into());}if !pending_calls.iter().any(|c|c["call_id"]==event["call_id"]){pending_calls.push(event.clone());}}
                        else{calls.push(event.clone());}
                    }
                    if self.id=="elevenlabs" && kind=="response.done"{calls=std::mem::take(&mut pending_calls);}
                    calls.extend(event.pointer("/toolCall/functionCalls").and_then(Value::as_array).cloned().unwrap_or_default());
                    for call in calls {
                        if tasks.len()>=16{return Err("Слишком много вызовов инструментов".into());}
                        let call_id=call[if self.id=="gemini"{"id"}else{"call_id"}].as_str().unwrap_or("").to_string();
                        if call_id.is_empty() || call_id.len()>256{return Err("Нет ID голосового инструмента".into());}
                        if !seen.insert(call_id.clone()){continue;}if seen.len()>1024{return Err("Слишком много инструментов в разговоре".into());}
                        let Some(tools)=tools.clone() else {continue;};
                        let id=self.id.clone();let declared=defs.iter().any(|d|d["name"]==call["name"]);self.dialog.working(1);
                        let handle=tasks.spawn(async move{
                            let name=call["name"].as_str().unwrap_or("").to_owned();
                            let arguments=call["arguments"].as_str().map(str::to_string).unwrap_or_else(||call["args"].to_string());
                            let limit = if name == "play_music" { 35 } else { 15 };
                            let result=if declared{
                                match tokio::time::timeout(Duration::from_secs(limit),tools.call(&name,&arguments)).await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        if name == "play_music" {
                                            tools.cancel_loading_music().await;
                                        }
                                        "Инструмент не ответил вовремя".into()
                                    }
                                }
                            }else{"Этот инструмент не был объявлен".into()};
                            let payload = match id.as_str(){"gemini"=>json!({"toolResponse":{"functionResponses":[{"id":call["id"],"name":name,"response":{"result":result}}]}}),_=>json!({"type":"conversation.item.create","item":{"type":"function_call_output","call_id":call["call_id"],"output":result}})};
                            (name, arguments, result, payload)
                        });
                        handles.insert(call_id,handle);
                    }
                    if speaking && ["session.output_audio.done","response.done"].contains(&kind){speaking=false;}
                }
            }
        }
    }
    async fn audio(&self,data:&str)->Result<(),String>{
        if self.cancelled.load(Ordering::SeqCst){return Ok(());}
        if data.len()>128000{return Err("Слишком большой кусок PCM".into());}
        let bytes=B64.decode(data).map_err(|_|"Неверный голосовой PCM")?;if bytes.len()%2!=0{return Err("Неверный голосовой PCM".into());}if bytes.is_empty(){return Ok(());}self.dialog.heard_voice();
        let output = self.output.lock().unwrap().clone();
        if let Some(output) = output {
            for bytes in bytes.chunks(2400) {
                let mut pcm = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
                loop {
                    if self.cancelled.load(Ordering::SeqCst) { return Ok(()); }
                    match output.try_send(pcm) {
                        Ok(()) => break,
                        Err(TrySendError::Full(pending)) if tokio::time::Instant::now() < deadline => pcm = pending,
                        Err(TrySendError::Full(_)) => return Err("Динамик не успевает принять голос".into()),
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
        Ok(())
    }
}
fn eleven_session(profile:&Profile,instructions:&str,tools:&[Value])->Value{
    let model=match profile.live_model.as_str(){"eleven_realtime_v1_max"=>"eleven_realtime_v1_max",_=>"eleven_realtime_v1_mini"};
    let voice=if profile.tts_voice.is_empty(){"brian"}else{&profile.tts_voice};
    json!({"type":"realtime","model":model,"instructions":instructions,"output_modalities":["audio"],"tools":tools,"max_duration_seconds":600,
        "audio":{"input":{"format":{"type":"audio/pcm","rate":24000},"transcription":{"language":"ru"},"turn_detection":{"type":"semantic_vad","eagerness":"medium","create_response":true,"interrupt_response":true}},"output":{"format":{"type":"audio/pcm"},"voice":voice}}})
}
#[cfg(test)]mod tests{
    use super::*;
    #[test]fn eleven_config_preserves_voice_instructions_and_tools(){let p=Profile{live_model:"eleven_realtime_v1_max".into(),tts_voice:"custom-voice".into(),..Default::default()};let config=eleven_session(&p,"Jarvis",&[json!({"type":"function","name":"test"})]);assert_eq!(config["model"],p.live_model);assert_eq!(config["audio"]["output"]["voice"],p.tts_voice);assert_eq!(config["audio"]["input"]["format"]["rate"],24000);assert_eq!(config["tools"][0]["name"],"test");assert_eq!(config["instructions"],"Jarvis");}
}
struct Link {
    provider: Runtime,
    id: Value,
    events: tokio::sync::mpsc::Receiver<Result<Value, String>>,
    receiver: tokio::task::JoinHandle<()>,
}

impl Link {
    async fn open(provider: Runtime, key: &str, session: Value) -> Result<Self, String> {
        let opened = provider.request("live.open", json!({"key":key,"session":session}), Duration::from_secs(20)).await?;
        let id = opened["id"].clone();
        let (tx, events) = tokio::sync::mpsc::channel(12);
        let receiver = tokio::spawn({
            let provider = provider.clone();
            let id = id.clone();
            async move {
                loop {
                    let reply = provider.request("live.recv", json!({"id":id}), Duration::from_secs(15)).await;
                    match reply {
                        Ok(value) if value.is_null() => break,
                        Ok(value) if value["idle"] == true => continue,
                        _ => {}
                    }
                    let end = reply.is_err();
                    if tx.send(reply).await.is_err() || end { break; }
                }
            }
        });
        Ok(Self { provider, id, events, receiver })
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.receiver.abort();
        let provider = self.provider.clone();
        let id = self.id.clone();
        tokio::spawn(async move { let _ = provider.notify("live.close", json!({"id":id})).await; });
    }
}

async fn send(socket:&Link,value:Value)->Result<(),String>{
    socket.provider.request("live.send", json!({"id":socket.id,"event":value}), Duration::from_secs(5)).await.map(|_| ())
}
impl Session for Voice {
    fn out_boost(&self)->f64{if self.id=="elevenlabs"{0.35}else{1.0}}
    fn feed(&self,pcm:&[i16]){queue_mic(&mut self.pending.lock().unwrap(),pcm);}
    fn take_audio(&self)->Option<Receiver<Vec<i16>>>{self.audio.lock().unwrap().take()}
    fn rate(&self)->usize{if self.id=="openai"{16000}else{24000}}
    fn ready(&self,timeout:Duration)->Result<(),String>{let Some(rx)=self.ready.lock().unwrap().take()else{return Ok(());};rx.recv_timeout(timeout).map_err(|_|"Голосовая сессия не открылась".to_string())?}
    fn say(&self,text:&str)->Result<(),String>{self.commands.try_send(text.to_string()).map_err(|e|e.to_string())}
    fn close(&self){self.cancelled.store(true,Ordering::SeqCst);self.close.notify_one();}
    fn dialog(&self)->&Dialog{&self.dialog}
    fn interrupted(&self)->bool{self.interrupted.swap(false,Ordering::SeqCst)}
}
