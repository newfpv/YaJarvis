<script module>
  const backgroundUrl = "/api/train/background";
  const cloudUrl = backgroundUrl + "/cloud";

  function formatSpace(mb) {
    if (!Number.isFinite(mb) || mb <= 0) return "";
    if (mb >= 1024) return `${(mb / 1024).toFixed(1)} ГБ`;
    return `${Math.round(mb)} МБ`;
  }
</script>

<script>
  import { onMount } from "svelte";
  import { api, icon, toast } from "#helpers/core.js";
  import styles from "./BackgroundRecording.module.css";

  let { onconnection } = $props();
  const controller = new AbortController();
  const { signal } = controller;

  let result = $state.raw(null);
  let pending = $state(false);
  let reading = $state(false);
  let error = $state("");
  let minutes = $state(60);
  let timer;
  let revision = 0;

  let account = $derived(result?.account || null);
  let login = $derived(result?.login || null);
  let accountTitle = $derived(account?.name || account?.login || "Яндекс Диск");
  let accountMeta = $derived.by(() => {
    if (!account) return "";
    const parts = [];
    if (account.login && account.login !== accountTitle) parts.push(account.login);
    const free = formatSpace(account.free_mb);
    const total = formatSpace(account.total_mb);
    if (free && total) parts.push(`свободно ${free} из ${total}`);
    else if (free) parts.push(`свободно ${free}`);
    parts.push("папка Приложения / YaJarvis");
    return parts.join(" · ");
  });

  let status = $derived.by(() => {
    if (!result) return "";
    if (result.error) return result.error;
    const { running, uploading, left_sec, sent, discarded } = result;
    if (!running && !sent && !discarded) return "";
    let activity = "";
    if (running) activity = uploading ? "Отправляю запись. " : `Записываю. Осталось ${Math.ceil(left_sec / 60)} мин. `;
    return `${activity}Архивов отправлено: ${sent}. Фрагментов пропущено: ${discarded}.`;
  });

  onMount(() => {
    void refresh();
    return () => {
      controller.abort();
      clearTimeout(timer);
    };
  });

  function update(next) {
    result = next;
    error = "";
    onconnection(Boolean(next.connected));
  }

  function scheduleRefresh() {
    clearTimeout(timer);
    if (!signal.aborted && (result?.running || result?.login?.code)) {
      timer = setTimeout(refresh, 2500);
    }
  }

  async function refresh() {
    if (pending) return;
    if (reading) {
      scheduleRefresh();
      return;
    }
    const currentRevision = revision;
    reading = true;
    try {
      const next = await api("GET", backgroundUrl, undefined, { signal });
      if (currentRevision === revision) update(next);
    } catch (failure) {
      if (!signal.aborted && currentRevision === revision) error = failure.message;
    } finally {
      reading = false;
      if (currentRevision === revision) scheduleRefresh();
    }
  }

  async function change(method, url, input) {
    if (pending) return false;
    pending = true;
    revision++;
    clearTimeout(timer);
    try {
      update(await api(method, url, input, { signal }));
      return true;
    } catch (failure) {
      if (!signal.aborted) toast(failure.message, true);
      return false;
    } finally {
      pending = false;
      scheduleRefresh();
    }
  }

  function connect() {
    void change("PUT", cloudUrl, {});
  }

  function disconnect() {
    if (result?.connected && !confirm("Отключить Яндекс Диск? Уже загруженные записи на Диске останутся.")) return;
    void change("DELETE", cloudUrl);
  }

  function start(event) {
    event.preventDefault();
    void change("POST", backgroundUrl, { minutes });
  }

  async function copyCode(code) {
    if (!code) return;
    try {
      await navigator.clipboard.writeText(code);
      toast("Код скопирован");
    } catch {
      toast("Выделите код вручную", true);
    }
  }
</script>

<section class="sec">
  <div class="sec-h">
    <h2>Яндекс Диск и запись фона</h2>
    <p>Колонка выгружает короткие записи и длительный фон в отдельную папку «Приложения / YaJarvis» на вашем Яндекс Диске.</p>
  </div>
  <div class="sec-b">
    {#if result?.connected}
      <div class="cn-ok">
        {@html icon("done")}
        <span>
          Яндекс Диск подключён · <b>{accountTitle}</b>
          {#if accountMeta}<small>{accountMeta}</small>{/if}
        </span>
      </div>
      {#if !result.running}
        <div class="row">
          <button type="button" class="btn ghost" disabled={pending} onclick={disconnect}>Выйти</button>
        </div>
      {/if}
      <form class={styles.form} onsubmit={start}>
        <div class="f">
          <label for="bg-length">Длительная запись фона (минутные порции без записи во флешку)</label>
          <select id="bg-length" bind:value={minutes} disabled={pending || result.running}>
            <option value={15}>15 минут</option>
            <option value={60}>1 час</option>
            <option value={240}>4 часа</option>
            <option value={480}>8 часов</option>
          </select>
        </div>
        <div class="row">
          {#if result.running}
            <button type="button" class="btn" disabled={pending} onclick={() => change("DELETE", backgroundUrl)}>Остановить</button>
          {:else}
            <button type="submit" class="btn primary" disabled={pending}>Записать фон</button>
          {/if}
        </div>
      </form>
    {:else if login?.code}
      <div class="ycode">
        <span>Откройте <a href={login.url || "https://ya.ru/device"} target="_blank" rel="noopener noreferrer">{(login.url || "https://ya.ru/device").replace("https://", "")}</a> и введите код</span>
        <b class="code-big" role="button" tabindex="0" title="Нажмите, чтобы скопировать"
          onclick={() => copyCode(login.code)}
          onkeydown={(event) => {
            if (event.key === "Enter" || event.key === " ") {
              event.preventDefault();
              void copyCode(login.code);
            }
          }}>{login.code}</b>
        <small class="muted">окно само увидит подключение</small>
      </div>
      <div class="row">
        <button type="button" class="btn ghost" disabled={pending} onclick={disconnect}>Отмена</button>
      </div>
    {:else if result}
      {#if login?.error}<p class="note err" role="status">{login.error}</p>{/if}
      <div class="row">
        <button type="button" class="btn primary" disabled={pending} onclick={connect}>Подключить Яндекс Диск</button>
      </div>
      <p class="note">Вход по коду в Яндекс ID. Колонка видит только свою папку «Приложения / YaJarvis» и не получает доступа к остальным файлам на Диске.</p>
    {:else if !error}
      <p class="note" role="status">Проверяю подключение…</p>
    {/if}
    {#if error || status}
      <p class="note" class:err={Boolean(error || result?.error)} role="status">{error || status}</p>
    {/if}
    {#if error && !result}
      <div><button type="button" class="btn" disabled={reading} onclick={refresh}>Повторить</button></div>
    {/if}
  </div>
</section>
