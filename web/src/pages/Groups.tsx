import { useState } from "react";
import {
  apiGet,
  apiSend,
  type Group,
  type HotMeme,
  type MemberProfile,
  type Memory,
  type PinnedMemory,
} from "../api";
import { ErrorBox, Loading, Notice, unixTime, useFetch } from "../ui";

const PACES = [
  { value: "addressed_only", label: "只应答（@/引用才回）" },
  { value: "quiet", label: "安静" },
  { value: "normal", label: "正常" },
  { value: "lively", label: "活跃" },
] as const;

type PanelKind = "profiles" | "pins" | "memories" | "hot_memes";

interface GroupsResponse {
  groups: Group[];
}

export default function Groups() {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<GroupsResponse>("/groups"),
  );
  const [notice, setNotice] = useState<string | null>(null);
  const [openPanel, setOpenPanel] = useState<{
    groupId: string;
    kind: PanelKind;
  } | null>(null);

  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const groups = data!.groups;
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

  function togglePanel(groupId: string, kind: PanelKind) {
    if (openPanel?.groupId === groupId && openPanel.kind === kind) {
      setOpenPanel(null);
    } else {
      setOpenPanel({ groupId, kind });
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
              activePanel={openPanel?.groupId === g.group_id ? openPanel.kind : null}
              onTogglePanel={(kind) => togglePanel(g.group_id, kind)}
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
                activePanel={openPanel?.groupId === g.group_id ? openPanel.kind : null}
                onTogglePanel={(kind) => togglePanel(g.group_id, kind)}
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
  activePanel,
  onTogglePanel,
}: {
  group: Group;
  onPatch: (g: Group, body: Record<string, unknown>, label: string) => Promise<void>;
  onReset: (g: Group) => Promise<void>;
  activePanel: PanelKind | null;
  onTogglePanel: (kind: PanelKind) => void;
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
        aria-expanded={activePanel === "profiles"}
        onClick={() => onTogglePanel("profiles")}
      >
        人物侧写
      </button>
      <button
        type="button"
        aria-expanded={activePanel === "pins"}
        onClick={() => onTogglePanel("pins")}
      >
        置顶备忘
      </button>
      <button
        type="button"
        aria-expanded={activePanel === "memories"}
        onClick={() => onTogglePanel("memories")}
      >
        群聊记忆
      </button>
      <button
        type="button"
        aria-expanded={activePanel === "hot_memes"}
        onClick={() => onTogglePanel("hot_memes")}
      >
        热梗记忆
      </button>
      {activePanel === "profiles" && <Profiles groupId={group.group_id} />}
      {activePanel === "pins" && <Pins groupId={group.group_id} />}
      {activePanel === "memories" && <Memories groupId={group.group_id} />}
      {activePanel === "hot_memes" && <HotMemes groupId={group.group_id} />}
    </li>
  );
}

function Profiles({ groupId }: { groupId: string }) {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<{ profiles: MemberProfile[] }>(`/groups/${groupId}/profiles`),
  );
  const [editingId, setEditingId] = useState<string | null>(null);
  const [form, setForm] = useState({
    display_name: "",
    identity_tags: "",
    interests: "",
    personality_notes: "",
    recent_status: "",
  });

  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const profiles = data!.profiles;

  function startEdit(p: MemberProfile) {
    setEditingId(p.user_id);
    setForm({
      display_name: p.display_name,
      identity_tags: p.identity_tags,
      interests: p.interests,
      personality_notes: p.personality_notes,
      recent_status: p.recent_status,
    });
  }

  async function saveEdit(userId: string) {
    await apiSend("PUT", `/groups/${groupId}/profiles/${encodeURIComponent(userId)}`, form);
    setEditingId(null);
    await reload();
  }

  return (
    <div className="memories">
      <p className="muted">
        群友四段式结构化侧写档案（共 {profiles.length} 人，发言时 1ms 直取注入，后台史官每 10 分钟自动精炼合并）
      </p>
      {profiles.length === 0 && <p className="muted">当前暂无群友侧写档案。</p>}
      <ul className="rows">
        {profiles.map((p) => (
          <li key={p.user_id} className="row wrap">
            {editingId === p.user_id ? (
              <div style={{ width: "100%", display: "grid", gap: "6px" }}>
                <strong>
                  编辑侧写：{p.display_name || p.user_id}（{p.speaker_ref ?? "未分配"}）
                </strong>
                <label>
                  称呼：
                  <input
                    value={form.display_name}
                    onChange={(e) => setForm({ ...form, display_name: e.target.value })}
                  />
                </label>
                <label>
                  身份标签：
                  <input
                    value={form.identity_tags}
                    onChange={(e) => setForm({ ...form, identity_tags: e.target.value })}
                  />
                </label>
                <label>
                  喜好与常聊：
                  <input
                    value={form.interests}
                    onChange={(e) => setForm({ ...form, interests: e.target.value })}
                  />
                </label>
                <label>
                  性格与印象：
                  <input
                    value={form.personality_notes}
                    onChange={(e) => setForm({ ...form, personality_notes: e.target.value })}
                  />
                </label>
                <label>
                  近期动态：
                  <input
                    value={form.recent_status}
                    onChange={(e) => setForm({ ...form, recent_status: e.target.value })}
                  />
                </label>
                <div>
                  <button type="button" onClick={() => void saveEdit(p.user_id)}>
                    保存
                  </button>{" "}
                  <button type="button" onClick={() => setEditingId(null)}>
                    取消
                  </button>
                </div>
              </div>
            ) : (
              <>
                <span>
                  <span className="tag">{p.speaker_ref ?? "群友"}</span>{" "}
                  <strong>{p.display_name || p.user_id}</strong>
                  <span className="muted">（QQ: {p.user_id}）</span>
                  <br />
                  <small>
                    身份：{p.identity_tags || "暂无"} ｜ 喜好：{p.interests || "暂无"} ｜ 性格印象：
                    {p.personality_notes || "暂无"} ｜ 近期动态：{p.recent_status || "暂无"}
                  </small>
                </span>
                <span>
                  <button type="button" onClick={() => startEdit(p)}>
                    编辑
                  </button>{" "}
                  <button
                    type="button"
                    onClick={() => {
                      if (window.confirm("删除该群友的侧写档案？")) {
                        void apiSend(
                          "DELETE",
                          `/groups/${groupId}/profiles/${encodeURIComponent(p.user_id)}`,
                        ).then(reload);
                      }
                    }}
                  >
                    删除
                  </button>
                </span>
              </>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

function Pins({ groupId }: { groupId: string }) {
  const { data, error, loading, reload } = useFetch(() =>
    apiGet<{ pins: PinnedMemory[] }>(`/groups/${groupId}/pins`),
  );
  const [newPin, setNewPin] = useState("");

  if (loading) return <Loading />;
  if (error) return <ErrorBox message={error} />;
  const pins = data!.pins;

  async function addPin(e: React.FormEvent) {
    e.preventDefault();
    if (!newPin.trim()) return;
    await apiSend("POST", `/groups/${groupId}/pins`, { content: newPin.trim() });
    setNewPin("");
    await reload();
  }

  return (
    <div className="memories">
      <p className="muted">
        置顶备忘录（共 {pins.length} 条，每轮对话固定置顶注入，用于存放主人的长期交代与永久自我约束）
      </p>
      <form onSubmit={(e) => void addPin(e)} className="row" style={{ marginBottom: "8px" }}>
        <input
          placeholder="新增置顶备忘（例如：群友考试周期间多给温和鼓励）"
          value={newPin}
          onChange={(e) => setNewPin(e.target.value)}
          style={{ flex: 1 }}
        />
        <button type="submit">添加置顶</button>
      </form>
      {pins.length === 0 && <p className="muted">暂无置顶备忘录。</p>}
      <ul className="rows">
        {pins.map((pin) => (
          <li key={pin.pin_ref} className="row wrap">
            <span>
              <span className="tag">置顶</span> {pin.content}
            </span>
            <button
              type="button"
              onClick={() => {
                if (window.confirm("删除这条置顶备忘？")) {
                  void apiSend(
                    "DELETE",
                    `/groups/${groupId}/pins/${encodeURIComponent(pin.pin_ref)}`,
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
      <p className="muted">
        群聊记忆（共 {memories.length} 条活跃记忆，仅包含群友日常事实、偏好、事件与话题）
      </p>
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
      <p className="muted">
        热梗记忆（共 {data!.total} 条活跃热梗，独立存储在 hot_memes 表，不占用群聊记忆名额）
      </p>
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
