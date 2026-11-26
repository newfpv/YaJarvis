<script>
  import { onMount } from "svelte";
  import { api } from "#helpers/core.js";
  let { accountId, usage = $bindable(null) } = $props();
  let loading = $state(false);
  let error = $state("");
  let alive = true;
  const windows = [["five_hour", "5 часов"], ["weekly", "Неделя"]];
  const percent = (value) => new Intl.NumberFormat("ru-RU", { maximumFractionDigits: 1 }).format(value);
  const date = (value) => new Date(value * 1000).toLocaleString("ru-RU", { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" });
  async function refresh() {
    if (loading) return;
    loading = true;
    try {
      const result = await api("POST", "/api/chatgpt/usage", {id: accountId});
      if (alive) { usage = result.usage; error = usage.error || ""; }
    } catch (e) {
      if (alive) {
        error = e.message;
        usage = { ...(usage || {}), error };
      }
    }
    finally { if (alive) loading = false; }
  }
  onMount(() => {
    refresh();
    const timer = setInterval(() => { if (!document.hidden) refresh(); }, 60000);
    return () => { alive = false; clearInterval(timer); };
  });
</script>
<div class="quota" aria-label="Лимиты Codex">
  <div class="quota-title"><b>Лимиты Codex</b><button type="button" class="btn sm" disabled={loading} onclick={refresh}>{loading ? "Проверяю…" : "Обновить лимиты"}</button></div>
  {#each windows as [key, label]}
    {@const item = usage?.windows?.[key]}
    <div class="quota-window">
      <div><span>{label}</span><b>{item ? `Осталось ${percent(item.remaining_percent)}%` : loading ? "Проверяю…" : "Нет данных"}</b></div>
      {#if item}
        <progress max="100" value={item.remaining_percent} aria-label={`${label}: осталось ${percent(item.remaining_percent)}%`}></progress>
        <small>Использовано {percent(item.used_percent)}% · {item.reset_at ? `Сброс ${date(item.reset_at)}` : "Время сброса не передано"}</small>
      {/if}
    </div>
  {/each}
  {#if error}<p class="err" role="status">{error}{usage?.fetched_at ? " Показаны последние полученные данные." : ""}</p>{/if}
  {#if usage?.fetched_at}<small>Получено {date(usage.fetched_at)} · обновляется раз в минуту</small>{/if}
  <p class="note">Это лимиты «мозга» Codex. Остаток голосового времени GPT-Live здесь не учитывается.</p>
</div>
<style>
  .quota {margin-top:14px;padding-top:12px;border-top:1px solid var(--line,#5555)}
  .quota-title,.quota-window>div{display:flex;justify-content:space-between;align-items:center;gap:10px;flex-wrap:wrap}
  .quota-window{margin:12px 0}.quota-window small,.quota>small{display:block;margin-top:4px;opacity:.75}
  progress{width:100%;height:8px;accent-color:var(--accent,#f87a25)}
  .quota .note,.quota .err{margin:8px 0 0}
</style>
