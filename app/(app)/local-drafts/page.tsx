'use client'

import PageHeader from '../components/PageHeader'
import { apiFetch, fetcher, handleResponse, put, sendRequest, type BiliType } from '@/app/lib/api-streamer'
import { defaultScheduledPublish, DTIME_MAX_DAYS, DTIME_MIN_HOURS } from '@/app/lib/publish'
import { useBiliUsers, useTypeTree } from '@/app/lib/use-streamers'
import { useMe } from '@/app/lib/use-me'
import {
  Banner,
  Button,
  Checkbox,
  DatePicker,
  Input,
  Layout,
  Radio,
  RadioGroup,
  Select,
  TabPane,
  Tabs,
  Tag,
  TagInput,
  TextArea,
  Toast,
  Typography,
} from '@douyinfe/semi-ui'
import { useMemo, useState } from 'react'
import useSWR from 'swr'

const { Content } = Layout
const { Text } = Typography

interface Draft {
  id: number
  file_name: string
  has_cover: boolean
  title: string
  description: string
  tags: string[]
  tid: number
  copyright: number
  source: string
  is_only_self: number
  statement_id: number
  no_reprint: number
  no_disturbance: number
  dtime: number | null
  submitted_at: number | null
  section_id: number | null
  user_id: number | null
  status: string
  bvid: string | null
  error: string | null
  needs_transcode: boolean
  video_codec: string | null
  audio_codec: string | null
  duration_ms: number | null
  width: number | null
  height: number | null
  bytes: number
}

interface DirectoryTag {
  id: number
  name: string
  tid: number | null
  user_id: number | null
  section_id: number | null
  tags: string[]
  statement_id: number | null
}

interface DraftList {
  directory: string
  directory_error: string | null
  drafts: Draft[]
  directory_tags: DirectoryTag[]
  active_tag_id: number | null
}

interface Season {
  id: number
  title: string
  sections: { id: number; title: string }[]
}

const STATUS: Record<string, { label: string; color: 'blue' | 'green' | 'orange' | 'red' | 'grey'; border: string }> = {
  ready: { label: '待发布', color: 'blue', border: 'var(--semi-color-primary)' },
  transcoding: { label: '转码中', color: 'orange', border: 'var(--semi-color-warning)' },
  uploading: { label: '上传中', color: 'orange', border: 'var(--semi-color-warning)' },
  submitted: { label: '已投稿', color: 'green', border: 'var(--semi-color-success)' },
  failed: { label: '失败', color: 'red', border: 'var(--semi-color-danger)' },
}

function formatBytes(bytes: number) {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

const STATEMENTS = [
  { id: -1, label: '内容无需标注' },
  { id: 1, label: '含AI生成内容' },
  { id: 2, label: '含虚构演绎内容' },
  { id: 3, label: '内容含营销信息' },
  { id: 4, label: '个人观点，仅供参考' },
  { id: -2, label: '内容为转载' },
]

function formatDuration(ms: number | null) {
  if (!ms) return ''
  const total = Math.round(ms / 1000)
  return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, '0')}`
}

export default function LocalDraftsPage() {
  const { can } = useMe()
  const canSubmit = can('upload.submit')
  const { data, error, isLoading, mutate } = useSWR<DraftList>('/v1/local-drafts', fetcher, {
    refreshInterval: latest =>
      latest?.drafts.some(item => item.status === 'transcoding' || item.status === 'uploading')
        ? 2000
        : 0,
  })
  const [directory, setDirectory] = useState<string>()
  const [tab, setTab] = useState('pending')
  const [tagEditor, setTagEditor] = useState(false)
  const { biliUsers } = useBiliUsers(canSubmit)
  const { typeTree } = useTypeTree()
  const tree = useMemo(() => normalizeTree(typeTree), [typeTree])
  const shownDirectory = directory ?? data?.directory ?? ''
  const grouped = useMemo(() => {
    const drafts = data?.drafts ?? []
    return {
      pending: drafts.filter(item => item.status !== 'submitted'),
      submitted: drafts.filter(item => item.status === 'submitted'),
    }
  }, [data?.drafts])

  const tags = data?.directory_tags ?? []

  async function chooseTag(value: string | number | undefined) {
    try {
      if (value == null || value === '') {
        const res = await apiFetch('/v1/local-drafts/tags/active', {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ id: null }),
        })
        await handleResponse(res)
      } else {
        const ids = (data?.drafts ?? [])
          .filter(item => item.status !== 'uploading' && item.status !== 'transcoding')
          .map(item => item.id)
        await sendRequest(`/v1/local-drafts/tags/${value}/apply`, { arg: { ids } })
        Toast.success('已按目录标签更新当前页')
      }
      await mutate()
    } catch (err) {
      Toast.error(err instanceof Error ? err.message : '套用目录标签失败')
    }
  }

  async function saveDirectory() {
    try {
      await put('/v1/local-drafts/settings', { arg: { directory: shownDirectory } })
      await mutate()
      Toast.success('已保存视频目录')
    } catch (err) {
      Toast.error(err instanceof Error ? err.message : '保存失败')
    }
  }

  return (
    <>
      <PageHeader
        title="本地待发布"
        description="查看目录里的视频和封面，改好标题、分区、创作声明和发布时间后发布。"
      />
      <Content style={{ padding: 24, backgroundColor: 'var(--semi-color-bg-0)' }}>
        <div style={{ display: 'flex', gap: 8, marginBottom: 16, flexWrap: 'wrap' }}>
          <Input
            value={shownDirectory}
            onChange={setDirectory}
            placeholder="视频目录"
            style={{ width: 480, maxWidth: '100%' }}
            disabled={!canSubmit}
          />
          <Button disabled={!canSubmit} onClick={saveDirectory}>
            保存目录
          </Button>
          <Button onClick={() => mutate()}>刷新</Button>
          <Select
            placeholder="目录标签"
            value={data?.active_tag_id ?? undefined}
            style={{ width: 180 }}
            showClear
            disabled={!canSubmit}
            optionList={tags.map(item => ({ value: item.id, label: item.name }))}
            onChange={value => chooseTag(value as number | undefined)}
          />
          <Button disabled={!canSubmit} onClick={() => setTagEditor(open => !open)}>{tagEditor ? '收起绑定' : '编辑绑定'}</Button>
        </div>
        {tagEditor && (
          <TagEditor
            tags={tags}
            activeId={data?.active_tag_id ?? null}
            tree={tree}
            users={biliUsers.map(user => ({ id: user.id, name: user.name }))}
            canSubmit={canSubmit}
            onChanged={() => mutate()}
          />
        )}
        {data?.directory_error && <Banner type="warning" description={data.directory_error} style={{ marginBottom: 16 }} />}
        {error && <Banner type="danger" description={error.message || '读取失败'} style={{ marginBottom: 16 }} />}
        {isLoading && <Text type="tertiary">正在扫描目录…</Text>}
        {!isLoading && data && !data.directory_error && (
          <Tabs type="line" activeKey={tab} lazyRender={false} onChange={key => setTab(String(key))}>
            <TabPane tab={`未发布 ${grouped.pending.length}`} itemKey="pending">
              <DraftStack drafts={grouped.pending} empty="没有未发布的视频。" canSubmit={canSubmit} onChanged={() => mutate()} />
            </TabPane>
            <TabPane tab={`已投稿 ${grouped.submitted.length}`} itemKey="submitted">
              <SubmittedList drafts={grouped.submitted} />
            </TabPane>
          </Tabs>
        )}
      </Content>
    </>
  )
}

function TagEditor({
  tags,
  activeId,
  tree,
  users,
  canSubmit,
  onChanged,
}: {
  tags: DirectoryTag[]
  activeId: number | null
  tree: { label: string; value: number; children?: { label: string; value: number }[] }[]
  users: { id: number; name: string }[]
  canSubmit: boolean
  onChanged: () => void
}) {
  const [picked, setPicked] = useState(activeId == null ? 'new' : String(activeId))
  const selected = tags.find(item => String(item.id) === picked) ?? null
  const [name, setName] = useState(selected?.name ?? '')
  const [tid, setTid] = useState(selected?.tid ?? 0)
  const [parentId, setParentId] = useState<number | undefined>()
  const [userId, setUserId] = useState<number | undefined>(selected?.user_id ?? undefined)
  const [sectionId, setSectionId] = useState<number | undefined>(selected?.section_id ?? undefined)
  const [boundTags, setBoundTags] = useState<string[]>(selected?.tags ?? [])
  const [statementId, setStatementId] = useState<number | undefined>(selected?.statement_id ?? undefined)
  const [saving, setSaving] = useState(false)
  const { data: seasons } = useSWR<Season[]>(
    canSubmit && userId ? `/v1/local-drafts/seasons?user_id=${userId}` : null,
    fetcher,
  )
  const sections = seasons?.flatMap(season =>
    season.sections.map(section => ({
      value: section.id,
      label: `${season.title} / ${section.title || '正片'}`,
    })),
  )

  function load(tag: DirectoryTag | null) {
    setName(tag?.name ?? '')
    setTid(tag?.tid ?? 0)
    setParentId(undefined)
    setUserId(tag?.user_id ?? undefined)
    setSectionId(tag?.section_id ?? undefined)
    setBoundTags(tag?.tags ?? [])
    setStatementId(tag?.statement_id ?? undefined)
  }

  async function save() {
    const body = {
      name,
      tid: tid > 0 ? tid : null,
      user_id: userId ?? null,
      section_id: sectionId ?? null,
      tags: boundTags,
      statement_id: statementId ?? null,
    }
    setSaving(true)
    try {
      const res = await apiFetch(picked === 'new' ? '/v1/local-drafts/tags' : `/v1/local-drafts/tags/${picked}`, {
        method: picked === 'new' ? 'POST' : 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      await handleResponse(res)
      const saved = await res.json() as DirectoryTag
      setPicked(String(saved.id))
      Toast.success('已保存目录标签')
      onChanged()
    } catch (err) {
      Toast.error(err instanceof Error ? err.message : '保存目录标签失败')
    } finally {
      setSaving(false)
    }
  }

  async function remove() {
    if (picked === 'new') return
    if (!window.confirm(`删除目录标签「${name || selected?.name || ''}」？`)) return
    const res = await apiFetch(`/v1/local-drafts/tags/${picked}`, { method: 'DELETE' })
    await handleResponse(res)
    setPicked('new')
    load(null)
    Toast.success('已删除目录标签')
    onChanged()
  }

  const parent = parentId ?? parentOf(tree, tid)
  return (
    <div style={{ display: 'grid', gap: 10, marginBottom: 16, padding: 12, border: '1px solid var(--semi-color-border)', borderRadius: 8 }}>
      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap', alignItems: 'center' }}>
        <Select
          placeholder="编辑哪个标签"
          value={picked}
          style={{ width: 180 }}
          disabled={!canSubmit || saving}
          optionList={[{ value: 'new', label: '新建标签' }, ...tags.map(item => ({ value: String(item.id), label: item.name }))]}
          onChange={value => {
            const next = value == null || value === 'new' ? 'new' : String(value)
            setPicked(next)
            load(next === 'new' ? null : tags.find(item => String(item.id) === next) ?? null)
          }}
        />
        <Input value={name} onChange={setName} placeholder="标签名称" style={{ width: 180 }} disabled={!canSubmit || saving} />
      </div>
      <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
        <Select
          placeholder="一级分区"
          value={parent}
          style={{ width: 140 }}
          showClear
          disabled={!canSubmit || saving}
          optionList={tree.map(item => ({ value: item.value, label: item.label }))}
          onChange={value => {
            if (value == null) {
              setParentId(undefined)
              setTid(0)
              return
            }
            const next = Number(value)
            setParentId(next)
            if (!tree.find(item => item.value === next)?.children?.some(child => child.value === tid)) setTid(0)
          }}
        />
        <Select
          placeholder="二级分区"
          value={childOf(tree, tid)}
          style={{ width: 180 }}
          showClear
          disabled={!canSubmit || saving || parent == null}
          optionList={(tree.find(item => item.value === parent)?.children ?? []).map(child => ({ value: child.value, label: child.label }))}
          onChange={value => setTid(value == null ? 0 : Number(value))}
        />
        <Select
          placeholder="投稿账号"
          value={userId}
          style={{ width: 220 }}
          showClear
          disabled={!canSubmit || saving}
          optionList={users.map(user => ({ value: user.id, label: user.name }))}
          onChange={value => {
            setUserId(value == null ? undefined : Number(value))
            setSectionId(undefined)
          }}
        />
        <Select
          placeholder="加入合集"
          value={sectionId}
          style={{ width: 280 }}
          showClear
          disabled={!canSubmit || saving || !userId}
          optionList={sections ?? []}
          onChange={value => setSectionId(value == null ? undefined : Number(value))}
        />
      </div>
      <TagInput value={boundTags} onChange={setBoundTags} max={12} placeholder="视频标签，回车添加。留空表示不改标签" disabled={!canSubmit || saving} />
      <Select
        placeholder="创作声明，留空表示不改"
        value={statementId}
        style={{ width: 280 }}
        showClear
        disabled={!canSubmit || saving}
        optionList={STATEMENTS.map(item => ({ value: item.id, label: item.label }))}
        onChange={value => setStatementId(value == null ? undefined : Number(value))}
      />
      <div style={{ display: 'flex', gap: 8 }}>
        <Button disabled={!canSubmit || saving} onClick={() => save().catch(() => undefined)}>保存标签</Button>
        {picked !== 'new' && (
          <Button type="danger" disabled={!canSubmit || saving} onClick={() => remove().catch(err => Toast.error(err instanceof Error ? err.message : '删除失败'))}>删除标签</Button>
        )}
      </div>
      <Text type="tertiary" size="small">留空的分区、账号、合集、标签和创作声明不会覆盖稿件里已有的值。在上方选中这个标签后，当前页稿件会改成这里绑定的内容。</Text>
    </div>
  )
}

function publishTime(draft: Draft) {
  const stamp = draft.dtime != null && draft.dtime > 0 ? draft.dtime : draft.submitted_at
  if (stamp == null || stamp <= 0) return '—'
  return new Date(stamp * 1000).toLocaleString('zh-CN', { hour12: false })
}

function SubmittedList({ drafts }: { drafts: Draft[] }) {
  if (drafts.length === 0) return <Text type="tertiary">还没有已投稿的视频。</Text>
  return (
    <div style={{ display: 'grid' }}>
      <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) 168px 72px 132px', gap: 12, padding: '8px 4px', color: 'var(--semi-color-text-2)', fontSize: 12 }}>
        <span>标题</span>
        <span>发布时间</span>
        <span>状态</span>
        <span>稿件</span>
      </div>
      {drafts.map(draft => {
        const status = STATUS[draft.status] ?? { label: draft.status, color: 'grey' as const }
        return (
          <div key={draft.id} style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) 168px 72px 132px', gap: 12, alignItems: 'center', padding: '10px 4px', borderTop: '1px solid var(--semi-color-border)' }}>
            <span title={draft.title} style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{draft.title || '未命名'}</span>
            <Text type="tertiary">{publishTime(draft)}</Text>
            <Tag size="small" color={status.color} type="light">{status.label}</Tag>
            {draft.bvid ? (
              <a href={`https://www.bilibili.com/video/${draft.bvid}`} target="_blank" rel="noreferrer">{draft.bvid}</a>
            ) : (
              <Text type="tertiary">暂无链接</Text>
            )}
          </div>
        )
      })}
    </div>
  )
}

function DraftStack({
  drafts,
  empty,
  canSubmit,
  onChanged,
}: {
  drafts: Draft[]
  empty: string
  canSubmit: boolean
  onChanged: () => void
}) {
  if (drafts.length === 0) return <Text type="tertiary">{empty}</Text>
  return (
    <div style={{ display: 'grid', gap: 16 }}>
      {drafts.map(draft => (
        <DraftCard key={`${draft.id}:${draft.status}:${draft.tid}:${draft.statement_id}:${draft.user_id ?? ''}:${draft.section_id ?? ''}:${draft.no_disturbance}:${draft.tags.join('\u0001')}:${draft.bvid ?? ''}:${draft.error ?? ''}`} draft={draft} canSubmit={canSubmit} onChanged={onChanged} />
      ))}
    </div>
  )
}

function DraftCard({
  draft,
  canSubmit,
  onChanged,
}: {
  draft: Draft
  canSubmit: boolean
  onChanged: () => void
}) {
  const { biliUsers } = useBiliUsers(canSubmit)
  const { typeTree } = useTypeTree()
  const tree = useMemo(() => normalizeTree(typeTree), [typeTree])
  const [title, setTitle] = useState(draft.title)
  const [description, setDescription] = useState(draft.description)
  const [tags, setTags] = useState(draft.tags)
  const [tid, setTid] = useState(draft.tid)
  const [parentId, setParentId] = useState<number | undefined>()
  const [source, setSource] = useState(draft.source)
  const [onlySelf, setOnlySelf] = useState(draft.is_only_self === 1)
  const [pushUpdate, setPushUpdate] = useState(draft.no_disturbance === 0)
  const [statementId, setStatementId] = useState(
    STATEMENTS.some(item => item.id === draft.statement_id) ? draft.statement_id : -1,
  )
  const [schedule, setSchedule] = useState(draft.dtime != null && draft.dtime > 0)
  const [dtime, setDtime] = useState<number | null>(() =>
    draft.dtime != null && draft.dtime > 0 ? draft.dtime : defaultScheduledPublish(Date.now(), draft.id),
  )
  const [sectionId, setSectionId] = useState<number | undefined>(draft.section_id ?? undefined)
  const [userId, setUserId] = useState<number | undefined>(draft.user_id ?? undefined)
  const [deleteAfter, setDeleteAfter] = useState(true)
  const [saving, setSaving] = useState(false)
  const reprint = statementId === -2
  const busy = draft.status === 'transcoding' || draft.status === 'uploading' || saving
  const { data: seasons } = useSWR<Season[]>(
    canSubmit && userId ? `/v1/local-drafts/seasons?user_id=${userId}` : null,
    fetcher,
  )
  const sections = seasons?.flatMap(season =>
    season.sections.map(section => ({
      value: section.id,
      label: `${season.title} / ${section.title || '正片'}`,
    })),
  )

  function payload() {
    return {
      title,
      description,
      tags,
      tid,
      copyright: reprint ? 2 : 1,
      source,
      is_only_self: onlySelf ? 1 : 0,
      statement_id: statementId,
      no_reprint: 0,
      no_disturbance: pushUpdate ? 0 : 1,
      dtime: schedule ? dtime : null,
      section_id: sectionId ?? null,
      user_id: userId ?? null,
    }
  }

  async function save() {
    if (schedule) {
      const stamp = dtime ?? 0
      const ahead = stamp * 1000 - Date.now()
      if (stamp <= 0) {
        Toast.error('请选择定时发送的时间')
        throw new Error('请选择定时发送的时间')
      }
      if (ahead < DTIME_MIN_HOURS * 3_600_000) {
        Toast.error(`定时发送要在 ${DTIME_MIN_HOURS} 小时之后`)
        throw new Error('定时发送太近')
      }
      if (ahead > DTIME_MAX_DAYS * 86_400_000) {
        Toast.error(`定时发送最多在 ${DTIME_MAX_DAYS} 天之内`)
        throw new Error('定时发送太远')
      }
    }
    setSaving(true)
    try {
      await put(`/v1/local-drafts/${draft.id}`, { arg: payload() })
      onChanged()
    } catch (err) {
      Toast.error(err instanceof Error ? err.message : '保存失败')
      throw err
    } finally {
      setSaving(false)
    }
  }

  async function publish() {
    try {
      await save()
    } catch {
      return
    }
    try {
      await sendRequest(`/v1/local-drafts/${draft.id}/publish`, { arg: { delete_after: deleteAfter } })
      Toast.success('已开始投稿')
      onChanged()
    } catch (err) {
      Toast.error(err instanceof Error ? err.message : '发布失败')
    }
  }

  async function dismiss() {
    const res = await apiFetch(`/v1/local-drafts/${draft.id}`, { method: 'DELETE' })
    await handleResponse(res)
    onChanged()
  }

  async function deleteFiles() {
    const cover = draft.has_cover ? '和封面' : ''
    if (!window.confirm(`删除「${draft.file_name}」${cover}原文件？文件删除后无法恢复。`)) return
    const res = await apiFetch(`/v1/local-drafts/${draft.id}?files=true`, { method: 'DELETE' })
    await handleResponse(res)
    Toast.success('已删除原文件')
    onChanged()
  }

  const meta = [
    formatBytes(draft.bytes),
    formatDuration(draft.duration_ms),
    draft.width && draft.height ? `${draft.width}×${draft.height}` : '',
    [draft.video_codec, draft.audio_codec].filter(Boolean).join(' + '),
  ]
    .filter(Boolean)
    .join(' · ')
  const status = STATUS[draft.status] ?? { label: draft.status, color: 'grey' as const, border: 'var(--semi-color-border)' }

  return (
    <section style={{ display: 'flex', flexWrap: 'wrap', gap: 16, padding: 16, border: '1px solid var(--semi-color-border)', borderLeft: `4px solid ${status.border}`, borderRadius: 8, background: draft.status === 'submitted' ? 'var(--semi-color-fill-0)' : 'transparent' }}>
      <div style={{ width: 220, maxWidth: '100%', flex: '0 0 220px' }}>
        {draft.has_cover ? (
          <img src={`/v1/local-drafts/${draft.id}/cover`} alt="" style={{ width: '100%', borderRadius: 6, background: 'var(--semi-color-fill-0)' }} />
        ) : (
          <div style={{ height: 124, borderRadius: 6, background: 'var(--semi-color-fill-0)' }} />
        )}
        <Text type="tertiary" size="small" style={{ display: 'block', marginTop: 8 }}>{draft.file_name}</Text>
        <Text size="small">{meta}</Text>
      </div>
      <div style={{ display: 'grid', gap: 10, flex: '1 1 320px', minWidth: 0 }}>
        <div style={{ display: 'flex', justifyContent: 'space-between', gap: 12 }}>
          <Tag size="large" color={status.color} type="light">{status.label}</Tag>
          {draft.bvid && (
            <a href={`https://www.bilibili.com/video/${draft.bvid}`} target="_blank" rel="noreferrer">{draft.bvid}</a>
          )}
        </div>
        {draft.error && <Banner type={draft.status === 'failed' ? 'danger' : 'warning'} description={draft.error} />}
        <Input value={title} onChange={setTitle} placeholder="标题" disabled={!canSubmit || busy} />
        <TextArea value={description} onChange={setDescription} placeholder="简介" autosize rows={2} disabled={!canSubmit || busy} />
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <Select
            placeholder="一级分区"
            value={parentId ?? parentOf(tree, tid)}
            style={{ width: 140 }}
            disabled={!canSubmit || busy}
            optionList={tree.map(item => ({ value: item.value, label: item.label }))}
            onChange={value => {
              const next = Number(value)
              const parent = tree.find(item => item.value === next)
              setParentId(next)
              if (!parent?.children?.some(child => child.value === tid)) setTid(0)
            }}
          />
          <Select
            placeholder="二级分区"
            value={childOf(tree, tid)}
            style={{ width: 180 }}
            disabled={!canSubmit || busy || (parentId ?? parentOf(tree, tid)) == null}
            optionList={(tree.find(item => item.value === (parentId ?? parentOf(tree, tid)))?.children ?? []).map(child => ({
              value: child.value,
              label: child.label,
            }))}
            onChange={value => setTid(Number(value))}
          />
          <Select
            placeholder="投稿账号"
            value={userId}
            style={{ width: 220 }}
            disabled={!canSubmit || busy}
            onChange={value => setUserId(Number(value))}
          >
            {biliUsers.map(user => (
              <Select.Option key={user.id} value={user.id}>{user.name}</Select.Option>
            ))}
          </Select>
          <Select
            placeholder="加入合集（可选）"
            value={sectionId}
            style={{ width: 280 }}
            showClear
            disabled={!canSubmit || busy || !userId}
            optionList={sections ?? []}
            onChange={value => setSectionId(value == null ? undefined : Number(value))}
          />
        </div>
        <TagInput value={tags} onChange={setTags} max={12} placeholder="标签，回车添加" disabled={!canSubmit || busy} />
        <div>
          <Text type="tertiary" size="small">创作声明</Text>
          <Select
            value={statementId}
            style={{ width: 280, display: 'block', marginTop: 4 }}
            disabled={!canSubmit || busy}
            optionList={STATEMENTS.map(item => ({ value: item.id, label: item.label }))}
            onChange={value => setStatementId(Number(value))}
          />
        </div>
        {reprint && (
          <Input value={source} onChange={setSource} placeholder="转载来源，例如 YouTube 链接" disabled={!canSubmit || busy} />
        )}
        <div>
          <Text type="tertiary" size="small">发送时间</Text>
          <RadioGroup
            value={schedule ? 'at' : 'now'}
            disabled={!canSubmit || busy}
            onChange={event => {
              const next = event.target.value === 'at'
              setSchedule(next)
              if (next) {
                setDtime(current => {
                  const earliest = Math.floor(Date.now() / 1000) + DTIME_MIN_HOURS * 3600 + 60
                  return current != null && current >= earliest ? current : defaultScheduledPublish(Date.now(), draft.id)
                })
              }
            }}
          >
            <Radio value="now">立即发送</Radio>
            <Radio value="at">定时发送</Radio>
          </RadioGroup>
          {schedule && (
            <DatePicker
              type="dateTime"
              value={dtime ? new Date(dtime * 1000) : undefined}
              disabled={!canSubmit || busy}
              format="yyyy-MM-dd HH:mm"
              style={{ width: 240, marginTop: 8 }}
              placeholder={`${DTIME_MIN_HOURS} 小时之后、${DTIME_MAX_DAYS} 天之内`}
              onChange={value => {
                const picked = value instanceof Date ? value : null
                setDtime(picked ? Math.floor(picked.getTime() / 1000) : null)
              }}
            />
          )}
        </div>
        <div style={{ display: 'flex', gap: 16, alignItems: 'center', flexWrap: 'wrap' }}>
          <Checkbox checked={pushUpdate} disabled={!canSubmit || busy} onChange={event => setPushUpdate(Boolean(event.target.checked))}>生成更新推送</Checkbox>
          <Checkbox checked={onlySelf} disabled={!canSubmit || busy} onChange={event => setOnlySelf(Boolean(event.target.checked))}>仅自己可见</Checkbox>
          <Checkbox checked={deleteAfter} disabled={!canSubmit || busy} onChange={event => setDeleteAfter(Boolean(event.target.checked))}>发布后删除原文件</Checkbox>
        </div>
        <div style={{ display: 'flex', gap: 8 }}>
          <Button disabled={!canSubmit || busy} onClick={() => save().catch(() => undefined)}>保存</Button>
          <Button theme="solid" disabled={!canSubmit || busy} onClick={publish}>发布</Button>
          <Button type="danger" disabled={!canSubmit || busy} onClick={() => dismiss().catch(err => Toast.error(err instanceof Error ? err.message : '移出失败'))}>移出列表</Button>
          <Button theme="solid" type="danger" disabled={!canSubmit || busy} onClick={() => deleteFiles().catch(err => Toast.error(err instanceof Error ? err.message : '删除失败'))}>删除原文件</Button>
        </div>
      </div>
    </section>
  )
}

function normalizeTree(types: { label: string; value: number; children?: BiliType[] }[] | undefined) {
  return (types ?? []).map(type => ({
    label: type.label,
    value: type.value,
    children: (type.children ?? []).map(child => ({ label: child.name, value: child.id })),
  }))
}

function parentOf(tree: { value: number; children?: { value: number }[] }[], tid: number) {
  for (const parent of tree) {
    if (parent.children?.some(child => child.value === tid)) return parent.value
  }
  return undefined
}

function childOf(tree: { value: number; children?: { value: number }[] }[], tid: number) {
  return parentOf(tree, tid) == null ? undefined : tid
}
