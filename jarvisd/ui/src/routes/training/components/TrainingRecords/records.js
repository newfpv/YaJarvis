export const recordsUrl = "/api/train/records";
export const clipKey = ({ storage, kind, name }) => [storage, kind, name].join("/");
export const clipUrl = ({ kind, name }) => recordsUrl + "/clip?" + new URLSearchParams({ kind, name });
export const clipWhen = ({ time }) => new Date(time).toLocaleString("ru-RU", {
  day: "numeric", month: "short", hour: "2-digit", minute: "2-digit",
});
export const clipProb = ({ name }) => name.match(/_p(\d+)/)?.[1];
export const clipTriggerMs = (clip) => {
  if (typeof clip?.trigger_ms === "number" && Number.isFinite(clip.trigger_ms) && clip.trigger_ms >= 0) {
    return clip.trigger_ms;
  }
  const match = clip?.name?.match(/_t(\d+)(?:_r)?\.wav$/);
  return match ? Number(match[1]) : null;
};
export const clipTriggerSec = (clip, duration = 0) => {
  const ms = clipTriggerMs(clip);
  if (ms !== null) {
    const sec = ms / 1000;
    return duration > 0 ? Math.max(0, Math.min(duration, sec)) : sec;
  }
  return duration > 0 ? Math.max(0, duration - 0.12) : 0;
};
export const clipStartSec = (clip, duration = 0) => {
  if (!(duration >= 1.2)) return 0;
  const trigger = clipTriggerSec(clip, duration);
  return Math.max(0, Math.min(duration * 0.5, trigger - 1.1));
};
export const clipSec = ({ size }) => Math.max(0, (size - 44) / 32000);
export const needsReview = ({ reviewed, kind }) => !reviewed && kind !== "bg";

const priority = { near: 0, pos: 1, false: 2 };

export function reviewQueue(clips, skipped) {
  return clips.filter((clip) => needsReview(clip) && !skipped.has(clipKey(clip)))
    .sort((a, b) => priority[a.kind] - priority[b.kind] || Date.parse(b.time) - Date.parse(a.time));
}
