//! Общее для всех голосовых движков: что движок разговора делает с сессией, и учёт хода разговора —
//! реплики в журнале, тишина, заглушение ответа.

use crate::journal::Journal;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    time::{Duration, Instant},
};

/// Открытый голосовой разговор с моделью. Микрофон — в feed, голос модели — из канала take_audio.
pub trait Session: Send + Sync {
    fn handles_commands(&self)->bool{false}
    /// Кадр микрофона, 16 кГц.
    fn feed(&self, pcm: &[i16]);
    /// Голос модели (забрать один раз); канал закрывается, когда разговор оборвался.
    fn take_audio(&self) -> Option<Receiver<Vec<i16>>>;
    /// Частота голоса модели.
    fn rate(&self) -> usize;
    fn ready(&self, timeout: Duration) -> Result<(), String>;
    /// Попросить произнести ровно эту фразу.
    fn say(&self, text: &str) -> Result<(), String>;
    fn close(&self);
    fn dialog(&self) -> &Dialog;
    /// Доп. усиление голоса движка (ChatGPT по подписке приходит тише).
    fn out_boost(&self) -> f64 {
        1.0
    }
    /// Модель сама услышала, что её перебили (сбрасывается при чтении).
    fn interrupted(&self) -> bool {
        false
    }
}

#[derive(Default)]
struct DState {
    last_activity: Option<Instant>,
    last_input: Option<Instant>,
    delegations: i32,
    turn: i64,
    turn_user: String,
    turn_bot: String,
    turn_bot_t0: Option<Instant>,
    turn_complete: bool,
    user_end: Option<Instant>,
    muted: bool,
    muted_at: Option<Instant>,
    user_turns: usize,
    model: String,
}

/// Учёт хода разговора; есть в сессии каждого движка.
pub struct Dialog {
    journal: Arc<Journal>,
    route: String,
    st: Mutex<DState>,
    close_requested: AtomicBool,
}

const PROMISES: &[&str] = &["поищу", "найду", "подберу", "включу", "включаю", "поставлю", "запущу"];

/// Последнее предложение ответа.
pub fn last_sentence(s: &str) -> &str {
    let s = s.trim_end_matches([' ', '.', '!', '…']);
    match s.rfind(['.', '!', '?', '…']) {
        Some(i) => &s[i + s[i..].chars().next().unwrap().len_utf8()..],
        None => s,
    }
}

impl Dialog {
    pub fn new(journal: Arc<Journal>, route: &str, model: &str) -> Dialog {
        Dialog {
            journal,
            route: route.into(),
            st: Mutex::new(DState { model: model.into(), last_activity: Some(Instant::now()), ..Default::default() }),
            close_requested: AtomicBool::new(false),
        }
    }

    pub fn request_close(&self) {
        self.close_requested.store(true, Ordering::SeqCst);
    }

    pub fn close_requested(&self) -> bool {
        self.close_requested.load(Ordering::SeqCst)
    }

    pub fn on_bot_note(&self, text: &str) {
        let note = text.trim();
        if note.is_empty() {
            return;
        }
        let mut st = self.st.lock().unwrap();
        if st.turn == 0 {
            self.next_turn(&mut st);
        }
        let existing = st.turn_bot.trim();
        if existing.is_empty() {
            st.turn_bot = note.into();
        } else if !existing.contains(note) {
            st.turn_bot = format!("{existing} {note}");
        }
        let answer = st.turn_bot.trim().to_string();
        self.journal.update(st.turn, |t| t.answer = answer);
    }

    pub fn set_model(&self, model: &str) {
        let mut st = self.st.lock().unwrap();
        if st.model == model { return; }
        st.model = model.into();
        self.journal.update(st.turn, |t| t.model = model.into());
    }

    fn next_turn(&self, st: &mut DState) {
        self.finish_turn_locked(st);
        st.turn = self.journal.new_turn(&self.route, &st.model);
        self.journal.update(st.turn, |t| t.live = true);
        st.turn_user.clear();
        st.turn_bot.clear();
        st.turn_bot_t0 = None;
        st.turn_complete = false;
    }

    fn finish_turn_locked(&self, st: &mut DState) {
        if st.turn != 0 {
            self.journal.update(st.turn, |t| t.live = false);
            self.journal.remember(st.turn);
        }
        st.turn = 0;
    }

    /// Очередной кусочек расшифровки речи человека.
    pub fn on_user(&self, d: &str) {
        let mut st = self.st.lock().unwrap();
        let now = Instant::now();
        st.last_activity = Some(now);
        st.user_end = Some(now);
        // новая реплика: Джарвис уже успел поговорить (>1.5 с) — значит, это не запоздавший хвост расшифровки
        if st.turn == 0 || st.turn_complete || st.turn_bot_t0.is_some_and(|t| now - t > Duration::from_millis(1500)) {
            self.next_turn(&mut st);
        }
        if st.turn_user.is_empty() {
            st.user_turns += 1;
        }
        st.turn_user.push_str(d);
        let text = st.turn_user.trim().to_string();
        self.journal.update(st.turn, |t| t.text = text);
        if st.muted && st.muted_at.is_some_and(|m| now - m > Duration::from_millis(600)) {
            st.muted = false; // человек договорил новую просьбу — следующий ответ уже слушаем
        }
    }

    pub fn provider_turn_complete(&self) {
        // Keep the text visible to the command handler until the next input arrives.
        self.st.lock().unwrap().turn_complete = true;
    }

    /// Очередной кусочек текста ответа модели.
    pub fn on_bot(&self, d: &str) {
        let mut st = self.st.lock().unwrap();
        st.last_activity = Some(Instant::now());
        if st.turn == 0 {
            self.next_turn(&mut st);
        }
        if st.turn_bot_t0.is_none() {
            st.turn_bot_t0 = Some(Instant::now());
        }
        st.turn_bot.push_str(d);
        let a = st.turn_bot.trim().to_string();
        self.journal.update(st.turn, |t| t.answer = a);
    }

    /// Модель произнесла слышимый звук: разговор жив; первый звук после реплики — задержка ответа.
    pub fn heard_voice(&self) {
        let mut st = self.st.lock().unwrap();
        st.last_activity = Some(Instant::now());
        if st.turn != 0
            && !st.turn_user.is_empty()
            && let Some(end) = st.user_end
        {
            let sec = (end.elapsed().as_secs_f64() * 100.0).round() / 100.0;
            self.journal.update(st.turn, |t| {
                if t.audio_s == 0.0 {
                    t.audio_s = sec
                }
            });
        }
    }

    /// Движок начал (+1) или закончил (−1) дело, во время которого молчание нормально.
    pub fn working(&self, delta: i32) {
        let mut st = self.st.lock().unwrap();
        st.delegations = (st.delegations + delta).max(0);
        st.last_activity = Some(Instant::now());
    }

    /// Текущая реплика человека: id, текст и сколько прошло с последнего кусочка расшифровки.
    pub fn utterance(&self) -> (i64, String, Duration) {
        let st = self.st.lock().unwrap();
        if st.turn == 0 || st.turn_user.is_empty() {
            return (0, String::new(), Duration::ZERO);
        }
        (st.turn, st.turn_user.trim().into(), st.user_end.map(|e| e.elapsed()).unwrap_or_default())
    }

    pub fn turn_text(&self) -> String {
        let st = self.st.lock().unwrap();
        self.journal.get(st.turn).map(|t| t.text).unwrap_or_default()
    }

    /// Текст реплики из быстрой расшифровки (движок мог не успеть).
    pub fn set_text(&self, text: &str) {
        let mut st = self.st.lock().unwrap();
        if st.turn == 0 {
            self.next_turn(&mut st);
        }
        self.journal.update(st.turn, |t| t.text = text.into());
    }

    /// Не играть ответ модели (с этого момента и до новой реплики человека).
    pub fn mute(&self) {
        let mut st = self.st.lock().unwrap();
        st.muted = true;
        st.muted_at = Some(Instant::now());
    }

    pub fn muted(&self) -> bool {
        self.st.lock().unwrap().muted
    }
    pub fn unmute(&self){self.st.lock().unwrap().muted=false;}

    /// Пометить текущую реплику в журнале (быстрая команда, стоп).
    pub fn annotate(&self, route: &str, answer: &str) {
        let st = self.st.lock().unwrap();
        self.journal.update(st.turn, |t| {
            t.route = route.into();
            if !answer.is_empty() {
                t.answer = answer.into();
            }
        });
    }

    /// Закрыть текущую реплику; err — что пошло не так (покажется в переписке).
    pub fn finish_turn(&self, err: &str) {
        let mut st = self.st.lock().unwrap();
        if !err.is_empty() {
            if st.turn == 0 {
                st.turn = self.journal.new_turn(&self.route, &st.model);
            }
            self.journal.update(st.turn, |t| t.error = err.into());
        }
        self.finish_turn_locked(&mut st);
    }

    /// Разговор закончен: обе стороны молчат idle_s и движок ничем не занят. Модель только что
    /// пообещала «сейчас поищу» — она ищет на своей стороне: ждём дольше.
    pub fn idle(&self, idle_s: f64) -> bool {
        self.idle_at(idle_s, Instant::now())
    }

    fn idle_at(&self, mut idle_s: f64, now: Instant) -> bool {
        let st = self.st.lock().unwrap();
        let bot = st.turn_bot.trim();
        if !bot.is_empty() && !bot.contains('?') {
            let last = last_sentence(bot).to_lowercase();
            if PROMISES.iter().any(|p| last.contains(p)) {
                idle_s = idle_s.max(25.0);
            }
        }
        st.delegations == 0
            && st.last_activity.max(st.last_input).is_none_or(|t| now.saturating_duration_since(t).as_secs_f64() > idle_s)
    }

    /// Local microphone activity can arrive well before the provider's transcription.
    pub fn heard_input(&self, at: Instant) {
        self.st.lock().unwrap().last_input = Some(at);
    }

    pub fn heard_input_since(&self, since: Instant) -> bool {
        self.st.lock().unwrap().last_input.is_some_and(|at| at >= since)
    }

    /// Сколько раз человек что-то сказал в этом разговоре.
    pub fn user_turns(&self) -> usize {
        self.st.lock().unwrap().user_turns
    }

    /// Разговор только что открылся: тишину считаем отсюда.
    pub fn touch(&self) {
        self.st.lock().unwrap().last_activity = Some(Instant::now());
    }
}

/// Очередь звука, пока соединение открывается: не больше 20 с, без потерь начала фразы.
pub fn queue_mic(pending: &mut Vec<i16>, pcm: &[i16]) {
    const CAP: usize = 20 * 16000;
    let n = pcm.len().min(CAP.saturating_sub(pending.len()));
    pending.extend_from_slice(&pcm[..n]);
}

pub fn take_mic(pending: &mut Vec<i16>, limit: usize) -> Vec<i16> {
    let n = pending.len().min(limit);
    pending.drain(..n).collect()
}

/// Адаптивный порог тишины с учётом программного усиления микрофона (`mic_boost_db`).
pub fn quiet_threshold(mic_boost_db: i64) -> u16 {
    if mic_boost_db <= 0 {
        return 80;
    }
    (80.0 * 10f64.powf(mic_boost_db.min(24) as f64 / 20.0))
        .min(450.0)
        .round() as u16
}

/// Catch up only across sustained background silence, keeping at least 300 ms.
/// Quiet speech on the station can peak at 90..700; never discard those frames.
pub fn take_mic_caught_up(pending: &mut Vec<i16>, limit: usize) -> Vec<i16> {
    take_mic_caught_up_with(pending, limit, quiet_threshold(0))
}

pub fn take_mic_caught_up_with(pending: &mut Vec<i16>, limit: usize, quiet_amp: u16) -> Vec<i16> {
    const FRAME: usize = 320;
    const QUIET: usize = 15;
    let mut skipped = 0;
    while pending.len().saturating_sub(skipped) >= (QUIET + 1) * FRAME
        && pending[skipped..skipped + QUIET * FRAME]
            .iter()
            .all(|v| v.unsigned_abs() <= quiet_amp)
    {
        skipped += FRAME;
    }
    if skipped > 0 {
        pending.drain(..skipped);
    }
    take_mic(pending, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]fn mic_catchup_keeps_soft_speech_and_the_end_of_a_pause(){
        let mut pcm=vec![0;16000*3];pcm.extend(vec![300;640]);let frame=take_mic_caught_up(&mut pcm,4800);
        assert_eq!(frame.len(),4800);assert_eq!(frame.iter().filter(|v|**v==300).count(),320);assert_eq!(pcm,vec![300;320]);
        let speech=vec![150;16000];let mut queue=speech.clone();assert_eq!(take_mic_caught_up(&mut queue,960),speech[..960]);assert_eq!(queue,speech[960..]);
    }

    #[test]
    fn adaptive_quiet_threshold_skips_boosted_room_noise_but_keeps_speech() {
        assert_eq!(quiet_threshold(0), 80);
        assert_eq!(quiet_threshold(18), 450);
        let mut quiet_speech = vec![100i16; 16000];
        assert_eq!(take_mic_caught_up(&mut quiet_speech, 960), vec![100i16; 960]);
        let mut pcm = vec![420i16; 16000 * 2];
        pcm.extend(vec![1200i16; 640]);
        let frame = take_mic_caught_up_with(&mut pcm, 4800, quiet_threshold(18));
        assert!(frame.iter().any(|&v| v == 1200));
        assert!(pcm.len() < 640);
    }

    #[test]
    fn dialog_turns() {
        let j = Arc::new(Journal::default());
        let d = Dialog::new(j.clone(), "r", "m");
        d.on_user("привет ");
        d.on_user("джарвис");
        d.on_bot("Сейчас поищу.");
        assert_eq!(d.utterance().1, "привет джарвис");
        assert_eq!(d.user_turns(), 1);
        assert!(!d.idle(0.0) || true);
        d.finish_turn("");
        let h = j.history();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].answer, "Сейчас поищу.");
        assert_eq!(last_sentence("Да. Сейчас включу!"), " Сейчас включу");
    }

    #[test]
    fn stop_after_a_short_reply_starts_its_own_turn() {
        let dialog = Dialog::new(Arc::new(Journal::default()), "gemini", "");
        dialog.on_user("Привет");
        dialog.on_bot("Привет!");
        let first = dialog.utterance().0;
        dialog.provider_turn_complete();
        assert_eq!(dialog.utterance().0, first);
        dialog.on_user("Заткнись");
        let (second, text, _) = dialog.utterance();
        assert_ne!(first, second);
        assert!(crate::fastcmd::is_stop(&text));
    }

    #[test]
    fn local_speech_extends_timeout_without_a_transcript() {
        let dialog = Dialog::new(Arc::new(Journal::default()), "Gemini", "");
        let start = Instant::now();
        dialog.st.lock().unwrap().last_activity = Some(start);
        assert!(dialog.idle_at(8.0, start + Duration::from_secs(9)));
        let input = start + Duration::from_secs(7);
        dialog.heard_input(input);
        assert!(dialog.heard_input_since(start));
        assert!(!dialog.heard_input_since(input + Duration::from_millis(1)));
        assert!(!dialog.idle_at(8.0, start + Duration::from_secs(9)));
        assert!(dialog.idle_at(8.0, start + Duration::from_secs(16)));
        assert_eq!(dialog.user_turns(), 0);
        assert_eq!(dialog.utterance().0, 0);
    }

    #[test]
    fn request_close_and_bot_note_update_turn() {
        let j = Arc::new(Journal::default());
        let d = Dialog::new(j.clone(), "r", "m");
        assert!(!d.close_requested());
        d.on_user("включи сало");
        d.on_bot("Ща врублю.");
        d.on_bot_note("Включил с YouTube: Сало.");
        d.request_close();
        assert!(d.close_requested());
        d.finish_turn("");
        let items = j.history();
        assert_eq!(items.len(), 1);
        assert!(items[0].answer.contains("Ща врублю."));
        assert!(items[0].answer.contains("Включил с YouTube: Сало."));
    }
}
