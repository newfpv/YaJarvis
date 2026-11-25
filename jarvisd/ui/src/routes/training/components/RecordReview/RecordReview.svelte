<script module>
  const barCount = 72;
  const emptyWave = Array(barCount).fill(12);
  const guess = {
    pos: "колонка решила, что звали",
    false: "колонка сработала, но решила, что зря",
    near: "колонка насторожилась, но не сработала",
  };

  async function waveform(url, signal) {
    const response = await fetch(url, { signal });
    if (!response.ok) return emptyWave;
    const bytes = await response.arrayBuffer();
    const context = new OfflineAudioContext(1, 1, 16000);
    const samples = (await context.decodeAudioData(bytes)).getChannelData(0);
    const peaks = emptyWave.map((_, i) => {
      const from = Math.floor(i * samples.length / barCount);
      const to = Math.floor((i + 1) * samples.length / barCount);
      let peak = 0;
      for (let sample = from; sample < to; sample++) peak = Math.max(peak, Math.abs(samples[sample]));
      return peak;
    });
    const top = Math.max(0.05, ...peaks);
    return peaks.map((peak) => Math.max(6, Math.round(peak * 100 / top)));
  }
</script>

<script>
  import { onMount } from "svelte";
  import { icon } from "#helpers/core.js";
  import { clipProb, clipStartSec, clipTriggerMs, clipTriggerSec, clipUrl, clipWhen } from "../TrainingRecords/records.js";
  import styles from "./RecordReview.module.css";

  let { clip, remaining, autoplay, archivePlaying, busy, onplay, onanswer } = $props();
  let audio = $state();
  let bars = $state.raw(emptyWave);
  let playing = $state(false);
  let wantPlay = $state(false);
  let playbackError = $state(false);
  let currentTime = $state(0);
  let duration = $state(0);
  let seekedInitial = false;
  let progress = $derived(duration ? Math.round(currentTime * barCount / duration) : 0);
  let exactTrigger = $derived(clipTriggerMs(clip) !== null);
  let triggerSec = $derived(clipTriggerSec(clip, duration));
  let startSec = $derived(clipStartSec(clip, duration));
  let triggerPct = $derived(duration > 0 ? Math.max(0, Math.min(99.5, (triggerSec / duration) * 100)) : 0);
  let zoneStartSec = $derived(duration > 0 ? Math.max(0, triggerSec - 0.9) : 0);
  let zoneLeftPct = $derived(duration > 0 ? (zoneStartSec / duration) * 100 : 0);
  let zoneWidthPct = $derived(duration > 0 ? Math.max(0, Math.min(100, (triggerSec / duration) * 100) - zoneLeftPct) : 0);

  onMount(() => {
    const controller = new AbortController();
    const media = audio;
    void waveform(clipUrl(clip), controller.signal).then((peaks) => {
      if (!controller.signal.aborted) bars = peaks;
    }).catch(() => {});
    if (autoplay) {
      wantPlay = true;
      if (media?.readyState >= 1) handleMetadata();
    }
    return () => {
      controller.abort();
      media.pause();
    };
  });

  $effect(() => {
    if (archivePlaying) {
      wantPlay = false;
      audio?.pause();
    }
  });

  function applyInitialStart(dur) {
    if (seekedInitial) return;
    seekedInitial = true;
    const start = clipStartSec(clip, dur);
    if (start > 0 && Math.abs(audio.currentTime - start) > 0.05) {
      audio.currentTime = start;
    }
    currentTime = audio.currentTime;
  }

  function handleMetadata() {
    const dur = Number.isFinite(audio.duration) ? audio.duration : 0;
    duration = dur;
    applyInitialStart(dur);
    if (wantPlay && audio.paused) {
      void audio.play().catch(() => {});
    }
  }

  function handleEnded() {
    const dur = duration || audio.duration || 0;
    const start = clipStartSec(clip, dur);
    audio.currentTime = start;
    currentTime = start;
    if (wantPlay) {
      void audio.play().catch(() => {});
    } else {
      playing = false;
    }
  }

  function play() {
    wantPlay = true;
    if (audio.readyState >= 1) {
      applyInitialStart(duration || audio.duration || 0);
      void audio.play().catch(() => {});
    }
  }

  function toggle() {
    if (audio.paused) {
      if (duration > 0 && audio.currentTime >= duration - 0.05) {
        audio.currentTime = startSec;
      }
      play();
    } else {
      wantPlay = false;
      audio.pause();
    }
  }

  function seek(event) {
    seekedInitial = true;
    audio.currentTime = Number(event.currentTarget.value);
    play();
  }

  function playFromZero() {
    seekedInitial = true;
    audio.currentTime = 0;
    play();
  }

  function handleKey(event) {
    if (event.defaultPrevented || event.repeat || event.altKey || event.ctrlKey || event.metaKey
        || event.target.closest?.("input, textarea, select, [contenteditable]")) return;
    if (event.key === "1") onanswer("pos");
    else if (event.key === "2") onanswer("false");
    else if (event.key === " " && !event.target.closest?.("button, a")) {
      event.preventDefault();
      toggle();
    }
  }
</script>

<svelte:window onkeydown={handleKey} />

<div class={styles.player}>
  <div class={styles.head}>
    <span class="chip k-{clip.kind} {styles.guess}">{guess[clip.kind]}{clipProb(clip) ? ` · ${clipProb(clip)}%` : ""}</span>
    {#if duration > 0}
      <span class="chip {styles.triggerChip}">триггер {exactTrigger ? "" : "~"}{triggerSec.toFixed(1)} с</span>
    {/if}
    <span class="spacer"></span>
    <span class="muted">{clipWhen(clip)} · осталось {remaining}</span>
  </div>
  <audio
    bind:this={audio}
    bind:currentTime
    bind:duration
    src={clipUrl(clip)}
    preload="auto"
    onloadedmetadata={handleMetadata}
    onended={handleEnded}
    onplaying={() => { playing = true; playbackError = false; onplay(); }}
    onpause={() => { playing = false; }}
    onerror={() => { playbackError = true; }}
  ></audio>
  <div class={styles.controls}>
    <button class="np-play" type="button" aria-label={playing ? "Пауза" : "Прослушать"} onclick={toggle}>
      {@html icon(playing ? "pause" : "play", "solid")}
    </button>
    <div class={styles.wave}>
      {#if duration > 0}
        <div class={styles.triggerZone} style:left={`${zoneLeftPct}%`} style:width={`${zoneWidthPct}%`} aria-hidden="true"></div>
      {/if}
      {#each bars as height, index}
        <i aria-hidden="true" class:filled={index < progress} style:--height={height + "%"}></i>
      {/each}
      {#if duration > 0}
        <div class={styles.triggerMark} style:left={`${triggerPct}%`} aria-hidden="true"></div>
      {/if}
      <input type="range" aria-label="Позиция в записи" min="0" max={duration || 0} step="0.01"
        value={currentTime} disabled={!duration} oninput={seek}>
    </div>
    <span class={styles.time}>{currentTime.toFixed(1)} / {duration ? duration.toFixed(1) : "…"} с</span>
  </div>
  {#if duration >= 1.2}
    <div class={styles.metaRow}>
      <span class="muted">Старт с {startSec.toFixed(1)} с (рядом с триггером)</span>
      <button class="btn sm ghost" type="button" onclick={playFromZero}>Сначала (0.0 с)</button>
    </div>
  {/if}
  {#if playbackError}<p class="note err" role="status">Не удалось воспроизвести запись.</p>{/if}
  <b class={styles.question}>Было слово «Джарвис»?</b>
  <div class={styles.answers}>
    <button class="btn primary" type="button" disabled={busy} onclick={() => onanswer("pos")}>Да, было <kbd>1</kbd></button>
    <button class="btn" type="button" disabled={busy} onclick={() => onanswer("false")}>Нет <kbd>2</kbd></button>
  </div>
  <div class={styles.actions}>
    <button class="btn sm ghost" type="button" disabled={busy} onclick={() => onanswer("skip")}>Пропустить</button>
    <button class="btn sm ghost danger" type="button" disabled={busy} onclick={() => onanswer("delete")}>Не уверен — удалить</button>
  </div>
</div>
