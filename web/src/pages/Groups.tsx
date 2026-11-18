import { useState } from "react";
import { apiGet, apiSend, type Group, type HotMeme, type Memory } from "../api";
import { ErrorBox, Loading, Notice, unixTime, useFetch } from "../ui";

const PACES = [
  { value: "addressed_only", label: "只应答（@/引用才回）" },
  { value: "quiet", label: "安静" },
  { value: "normal", label: "正常" },
  { value: "lively", label: "活跃" },
] as const;

interface GroupsResponse {
  groups: Group[];
}

export default function Groups() {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<GroupsResponse>("/groups"),
  );
  const [notice, setNotice] = useState<string | null>(null);
  const [openMemories, setOpenMemories] = useState<string | null>(null);
  const [openHotMemes, setOpenHotMemes] = useState<string | null>(null);

  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const groups = data!.groups;
  // A discovered group starts disabled with no conversation and no turns;
  // a group an operator switched off has run before. That is the only
  // signal separating "pending enable" from "disabled".
  const neverRan = (g: Group) => !g.has_conversation && g.agy_turn_count === 0;
  const pending = groups.filter((g) => !g.enabled && !g.departed_at && neverRan(g));
  const departed = groups.filter((g) => g.departed_at);
  const active = groups.filter((g) => g.enabled && !g.departed_at);
  const disabled = groups.filter((g) => !g.enabled && !g.departed_at && !neverRan(g));

  async function patch(group: Group, body: Record<string, unknown>, label: string) {
    try {
      await apiSend("PATCH", `/groups/${group.group_id}`, body);
      setNotice(`${label}已保存（群 ${group.name ?? group.group_id}）`);
      await reload();
    } catch {
      setNotice(`${label}保存失败`);
    }
  }

  async function reset(group: Group) {
    try {
      await apiSend("POST", `/groups/${group.group_id}/session/reset`);
      setNotice(`会话已重置（群 ${group.name ?? group.group_id}），下一轮开新会话`);
      await reload();
    } catch {
      setNotice("会话重置失败");
    }
  }

  return (
    <div>
      <Notice text={notice} />
      {pending.length > 0 && (
        <section aria-labelledby="g-pending" className="card highlight">
          <h2 id="g-pending">已发现待启用（{pending.length}）</h2>
          <p className="muted">
            这些群已经被发现并登记，但机器人还不会在里面发言。选好参与档位再启用。
          </p>
          <ul className="rows">
            {pending.map((g) => (
              <PendingRow key={g.group_id} group={g} onEnable={patch} />
            ))}
          </ul>
        </section>
      )}

      <section aria-labelledby="g-active" className="card">
        <h2 id="g-active">服务中的群（{active.length}）</h2>
        <ul className="rows">
          {active.map((g) => (
            <GroupRow
              key={g.group_id}
              group={g}
              onPatch={patch}
              onReset={reset}
              memoriesOpen={openMemories === g.group_id}
              onToggleMemories={() => {
                setOpenHotMemes(null);
                setOpenMemories(openMemories === g.group_id ? null : g.group_id);
              }}
              hotMemesOpen={openHotMemes === g.group_id}
              onToggleHotMemes={() => {
                setOpenMemories(null);
                setOpenHotMemes(openHotMemes === g.group_id ? null : g.group_id);
              }}
            />
          ))}
        </ul>
        {active.length === 0 && <p className="muted">暂无。</p>}
      </section>

      {disabled.length > 0 && (
        <section aria-labelledby="g-disabled" className="card">
          <h2 id="g-disabled">已停用（{disabled.length}）</h2>
          <ul className="rows">
            {disabled.map((g) => (
              <GroupRow
                key={g.group_id}
                group={g}
                onPatch={patch}
                onReset={reset}
                memoriesOpen={openMemories === g.group_id}
                onToggleMemories={() => {
                  setOpenHotMemes(null);
                  setOpenMemories(openMemories === g.group_id ? null : g.group_id);
                }}
                hotMemesOpen={openHotMemes === g.group_id}
                onToggleHotMemes={() => {
                  setOpenMemories(null);
                  setOpenHotMemes(openHotMemes === g.group_id ? null : g.group_id);
                }}
              />
            ))}
          </ul>
        </section>
      )}

      {departed.length > 0 && (
        <section aria-labelledby="g-departed" className="card">
          <h2 id="g-departed">已被移出的群（{departed.length}）</h2>
          <ul className="rows">
            {departed.map((g) => (
              <li key={g.group_id}>
                {g.name ?? g.group_id}（{g.group_id}）· 移出于 {unixTime(g.departed_at)}
                <span className="muted"> · 数据保留，未删除</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

function PendingRow({
  group,
  onEnable,
}: {
  group: Group;
  onEnable: (g: Group, body: Record<string, unknown>, label: string) => Promise<void>;
}) {
  const [pace, setPace] = useState("normal");
  return (
    <li className="row">
      <span>
        <strong>{group.name ?? "（未取到群名）"}</strong> · {group.group_id}
        {group.member_count !== null && (
          <span className="muted"> · {group.member_count} 人</span>
        )}
      </span>
      <label>
        参与档位
        <select
          value={pace}
          onChange={(e) => setPace(e.target.value)}
          aria-label={`群 ${group.group_id} 的参与档位`}
        >
          {PACES.map((p) => (
            <option key={p.value} value={p.value}>
              {p.label}
            </option>
          ))}
        </select>
      </label>
      <button
        type="button"
        onClick={() => void onEnable(group, { enabled: true, ambient_pace: pace }, "启用")}
      >
        启用
      </button>
    </li>
  );
}

function GroupRow({
  group,
  onPatch,
  onReset,
  memoriesOpen,
  onToggleMemories,
  hotMemesOpen,
  onToggleHotMemes,
}: {
  group: Group;
  onPatch: (g: Group, body: Record<string, unknown>, label: string) => Promise<void>;
  onReset: (g: Group) => Promise<void>;
  memoriesOpen: boolean;
  onToggleMemories: () => void;
  hotMemesOpen: boolean;
  onToggleHotMemes: () => void;
}) {
  return (
    <li className="row wrap">
      <span>
        <strong>{group.name ?? group.group_id}</strong>
        <span className="muted">
          {" "}
          · {group.group_id} · 会话轮次 {group.agy_turn_count}/
          {group.conversation_turn_limit ?? "不限制"}
        </span>
      </span>
      <label>
        <input
          type="checkbox"
          checked={group.enabled}
          onChange={(e) => void onPatch(group, { enabled: e.target.checked }, "启用状态")}
        />
        启用
      </label>
      <label>
        <input
          type="checkbox"
          checked={group.paused}
          onChange={(e) => void onPatch(group, { paused: e.target.checked }, "暂停状态")}
        />
        暂停
      </label>
      <label>
        档位
        <select
          value={group.ambient_pace}
          onChange={(e) => void onPatch(group, { ambient_pace: e.target.value }, "参与档位")}
          aria-label={`群 ${group.group_id} 的参与档位`}
        >
          {PACES.map((p) => (
            <option key={p.value} value={p.value}>
              {p.label}
            </option>
          ))}
        </select>
      </label>
      <button
        type="button"
        onClick={() => {
          if (window.confirm(`重置群 ${group.group_id} 的会话？下一轮将开新会话。`)) {
            void onReset(group);
          }
        }}
      >
        重置会话
      </button>
      <button
        type="button"
        aria-expanded={memoriesOpen}
        onClick={onToggleMemories}
      >
        群聊记忆
      </button>
      <button
        type="button"
        aria-expanded={hotMemesOpen}
        onClick={onToggleHotMemes}
      >
        热梗记忆
      </button>
      {memoriesOpen && <Memories groupId={group.group_id} />}
      {hotMemesOpen && <HotMemes groupId={group.group_id} />}
    </li>
  );
}

function Memories({ groupId }: { groupId: string }) {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<{ memories: Memory[]; total: number }>(
      `/groups/${groupId}/memories?limit=100`,
    ),
  );
  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const memories = data!.memories.filter((m) => !m.archived_at);
  return (
    <div className="memories">
      <p className="muted">群聊记忆（共 {memories.length} 条活跃记忆，仅包含群友日常事实、偏好、事件与话题）</p>
      {memories.length === 0 && <p className="muted">这个群还没有群聊记忆。</p>}
      <ul className="rows">
        {memories.map((m) => (
          <li key={m.memory_ref} className="row wrap">
            <span>
              <span className="tag">{m.kind}</span> {m.content}
            </span>
            <button
              type="button"
              onClick={() => {
                if (window.confirm("删除这条群聊记忆？")) {
                  void apiSend(
                    "DELETE",
                    `/groups/${groupId}/memories/${encodeURIComponent(m.memory_ref)}`,
                  ).then(reload);
                }
              }}
            >
              删除
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

const SOURCE_LABELS: Record<string, string> = {
  trending_sync: "全网热搜",
  heybox_sync: "小黑盒热榜",
  heybox_lingo: "玩家黑话",
};

function HotMemes({ groupId }: { groupId: string }) {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<{ hot_memes: HotMeme[]; total: number }>(
      `/groups/${groupId}/hot-memes?limit=100`,
    ),
  );
  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const hotMemes = data!.hot_memes;
  return (
    <div className="memories">
      <p className="muted">热梗记忆（共 {data!.total} 条活跃热梗，独立存储在 hot_memes 表，不占用群聊记忆名额）</p>
      {hotMemes.length === 0 && <p className="muted">当前暂无同步的热梗记忆。</p>}
      <ul className="rows">
        {hotMemes.map((hm) => (
          <li key={hm.meme_ref} className="row wrap">
            <span>
              <span className="tag">{SOURCE_LABELS[hm.source] ?? hm.source}</span>{" "}
              {hm.content}
            </span>
            <button
              type="button"
              onClick={() => {
                if (window.confirm("删除这条热梗记忆？")) {
                  void apiSend(
                    "DELETE",
                    `/groups/${groupId}/hot-memes/${encodeURIComponent(hm.meme_ref)}`,
                  ).then(reload);
                }
              }}
            >
              删除
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}
