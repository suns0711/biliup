'use client'

import PageHeader from '../components/PageHeader'
import { apiFetch, fetcher, handleResponse, put, sendRequest, type BiliType } from '@/app/lib/api-streamer'
import { useBiliUsers, useTypeTree } from '@/app/lib/use-streamers'
import { useMe } from '@/app/lib/use-me'
import {
  Banner,
  Button,
  Cascader,
  Checkbox,
  Input,
  Layout,
  Radio,
  RadioGroup,
  Select,
  TagInput,
  TextArea,
  Toast,
  Typography,
} from '@douyinfe/semi-ui'
import { useMemo, useState } from 'react'
import useSWR from 'swr'

const { Content } = Layout
const { Text, Title } = Typography

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

interface DraftList {
  directory: string
  directory_error: string | null
  drafts: Draft[]
}

interface Season {
  id: number
  title: string
  sections: { id: number; title: string }[]
}

const STATUS: Record<string, string> = {
  ready: '待发布',
  transcoding: '转码中',
  uploading: '上传中',
  submitted: '已投稿',
  failed: '失败',
}

function formatBytes(bytes: number) {
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

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
  const shownDirectory = directory ?? data?.directory ?? ''

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
        description="查看目录里的视频和封面，改好标题、分区、标签和合集后发布。AV1 / Opus 会先转成 H.264 + AAC。"
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
        </div>
        {data?.directory_error && <Banner type="warning" description={data.directory_error} style={{ marginBottom: 16 }} />}
        {error && <Banner type="danger" description={error.message || '读取失败'} style={{ marginBottom: 16 }} />}
        {isLoading && <Text type="tertiary">正在扫描目录…</Text>}
        {!isLoading && data && !data.directory_error && data.drafts.length === 0 && (
          <Text type="tertiary">这个目录里没有待发布的视频。</Text>
        )}
        <div style={{ display: 'grid', gap: 16 }}>
          {data?.drafts.map(draft => (
            <DraftCard key={`${draft.id}:${draft.status}:${draft.bvid ?? ''}:${draft.error ?? ''}`} draft={draft} canSubmit={canSubmit} onChanged={() => mutate()} />
          ))}
        </div>
      </Content>
    </>
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
  const [copyright, setCopyright] = useState(draft.copyright)
  const [source, setSource] = useState(draft.source)
  const [onlySelf, setOnlySelf] = useState(draft.is_only_self === 1)
  const [sectionId, setSectionId] = useState<number | undefined>(draft.section_id ?? undefined)
  const [userId, setUserId] = useState<number | undefined>(draft.user_id ?? undefined)
  const [moveAfter, setMoveAfter] = useState(false)
  const [saving, setSaving] = useState(false)
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
      copyright,
      source,
      is_only_self: onlySelf ? 1 : 0,
      section_id: sectionId ?? null,
      user_id: userId ?? null,
    }
  }

  async function save() {
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
      await sendRequest(`/v1/local-drafts/${draft.id}/publish`, { arg: { move_after: moveAfter } })
      Toast.success(draft.needs_transcode ? '已开始转码并投稿' : '已开始投稿')
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

  const meta = [
    formatBytes(draft.bytes),
    formatDuration(draft.duration_ms),
    draft.width && draft.height ? `${draft.width}×${draft.height}` : '',
    [draft.video_codec, draft.audio_codec].filter(Boolean).join(' + '),
  ]
    .filter(Boolean)
    .join(' · ')

  return (
    <section style={{ display: 'flex', flexWrap: 'wrap', gap: 16, padding: 16, border: '1px solid var(--semi-color-border)', borderRadius: 8 }}>
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
          <Title heading={6} style={{ margin: 0 }}>{STATUS[draft.status] ?? draft.status}</Title>
          {draft.bvid && (
            <a href={`https://www.bilibili.com/video/${draft.bvid}`} target="_blank" rel="noreferrer">{draft.bvid}</a>
          )}
        </div>
        {draft.needs_transcode && draft.status !== 'submitted' && (
          <Banner type="warning" description="这个文件是 AV1、VP9 或 Opus，发布前会转成 H.264 + AAC，否则 B 站可能一直转码。" />
        )}
        {draft.error && <Banner type={draft.status === 'failed' ? 'danger' : 'warning'} description={draft.error} />}
        <Input value={title} onChange={setTitle} placeholder="标题" disabled={!canSubmit || busy} />
        <TextArea value={description} onChange={setDescription} placeholder="简介" autosize rows={2} disabled={!canSubmit || busy} />
        <div style={{ display: 'flex', gap: 8, flexWrap: 'wrap' }}>
          <Cascader
            treeData={tree}
            value={tidPath(tree, tid)}
            placeholder="分区"
            style={{ width: 240 }}
            disabled={!canSubmit || busy}
            onChange={value => {
              const picked = Array.isArray(value) ? value[value.length - 1] : value
              if (typeof picked === 'number') setTid(picked)
            }}
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
        <RadioGroup
          value={copyright}
          disabled={!canSubmit || busy}
          onChange={event => setCopyright(Number(event.target.value))}
        >
          <Radio value={1}>自制</Radio>
          <Radio value={2}>转载</Radio>
        </RadioGroup>
        {copyright === 2 && (
          <Input value={source} onChange={setSource} placeholder="转载来源，例如 YouTube 链接" disabled={!canSubmit || busy} />
        )}
        <div style={{ display: 'flex', gap: 16, alignItems: 'center', flexWrap: 'wrap' }}>
          <Checkbox checked={onlySelf} disabled={!canSubmit || busy} onChange={event => setOnlySelf(Boolean(event.target.checked))}>仅自己可见</Checkbox>
          <Checkbox checked={moveAfter} disabled={!canSubmit || busy} onChange={event => setMoveAfter(Boolean(event.target.checked))}>发布后移到 published</Checkbox>
        </div>
        <div style={{ display: 'flex', gap: 8 }}>
          <Button disabled={!canSubmit || busy} onClick={() => save().catch(() => undefined)}>保存</Button>
          <Button theme="solid" disabled={!canSubmit || busy} onClick={publish}>发布</Button>
          <Button type="danger" disabled={!canSubmit || busy} onClick={() => dismiss().catch(err => Toast.error(err instanceof Error ? err.message : '移出失败'))}>移出列表</Button>
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

function tidPath(tree: { value: number; children?: { value: number }[] }[], tid: number) {
  for (const parent of tree) {
    for (const child of parent.children ?? []) {
      if (child.value === tid) return [parent.value, child.value]
    }
    if (parent.value === tid) return [parent.value]
  }
  return []
}
