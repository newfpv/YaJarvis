<script>
  import { onMount } from "svelte";
  import { api } from "#helpers/core.js";
  let { stats: initial, accountId } = $props();
  let current = $state(null);
  let stats = $derived(current || initial);
  let error = $state("");
  onMount(() => {
    const controller = new AbortController();let busy=false;
    async function refresh(){
      if (busy || document.hidden) return;busy=true;
      try {const result=await api("GET", `/api/chatgpt/live-stats?id=${encodeURIComponent(accountId)}`, undefined, {signal:controller.signal});current=result.stats;error="";}
      catch(e){if(!controller.signal.aborted)error="Статистика не обновилась. Показаны последние полученные данные.";}
      finally{busy=false;}
    }
    refresh();const timer=setInterval(refresh,15000);
    return ()=>{controller.abort();clearInterval(timer);};
  });
  const duration = (ms = 0) => {
    const seconds = Math.floor(ms / 1000), minutes = Math.floor(seconds / 60), hours = Math.floor(minutes / 60);
    if (hours) return `${hours} ч ${minutes % 60} мин ${seconds % 60} с`;
    return minutes ? `${minutes} мин ${seconds % 60} с` : `${seconds} с`;
  };
  const date = (t) => new Date(t * 1000).toLocaleString("ru-RU", {day:"numeric",month:"short",hour:"2-digit",minute:"2-digit"});
</script>
<section class="live-stats" aria-label="Статистика GPT-Live на колонке">
  <b>GPT-Live · на этой колонке</b>
  {#if error}<p class="err" role="status">{error}</p>{/if}
  <dl>
    <div><dt>Голосовых соединений</dt><dd>{stats?.sessions || 0}</dd></div>
    <div><dt>Соединение открыто</dt><dd>{duration(stats?.connected_ms)}</dd></div>
    <div><dt>Аудио отправлено</dt><dd>{duration(stats?.sent_ms)}</dd></div>
    <div><dt>Аудио получено</dt><dd>{duration(stats?.received_ms)}</dd></div>
    <div><dt>Отказов из-за лимита</dt><dd>{stats?.limit_count || 0}</dd></div>
  </dl>
  {#if stats?.since}
    <small>Учёт с {date(stats.since)} · сохранено {date(stats.updated_at)}</small>
  {:else}<small>Учёт начнётся со следующего соединения. Прошлые разговоры не восстановлены.</small>{/if}
  <p class="note">Соединение может оставаться открытым в ожидании. Аудиопотоки включают тишину. Это измерения колонки, а не списанные минуты или остаток квоты OpenAI. Разговоры на других устройствах не учитываются. Сохранение — раз в минуту и при закрытии.</p>
  {#if stats?.limit_events?.length}
    <details><summary>Последние сообщения о лимите</summary>
      <ul>{#each stats.limit_events as event}<li>{date(event.at)} — {event.message}</li>{/each}</ul>
    </details>
  {/if}
</section>
<style>
  .live-stats{margin-top:16px;padding-top:14px;border-top:1px solid var(--line,#5555)}
  dl{margin:12px 0}dl>div{display:flex;justify-content:space-between;gap:12px;margin:6px 0;flex-wrap:wrap}dt{opacity:.8}dd{margin:0;font-variant-numeric:tabular-nums}
  small{opacity:.7}p{margin:8px 0}details{margin-top:10px}ul{padding-left:18px}li{margin:6px 0}
</style>
