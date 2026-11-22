import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, client, downloadResult } from '../src/api';
import { envelopeSchema } from '../src/contracts';
import { jsonResponse, errorResponse, statusFixture } from './fixtures';

afterEach(() => vi.unstubAllGlobals());
describe('HTTP 契约', () => {
  it('只调用新路由，切路由不传旧 mode', async () => {
    const fetcher = vi.fn().mockResolvedValue(jsonResponse({ route: 'rvc' })); vi.stubGlobal('fetch', fetcher);
    await client.route('rvc');
    expect(fetcher).toHaveBeenCalledOnce();
    expect(fetcher.mock.calls[0][0]).toBe('/api/v1/route');
    expect(fetcher.mock.calls[0][1]).toMatchObject({ method: 'PUT', body: '{"route":"rvc"}' });
  });
  it('读取四字段信封，保留期望路由和实际路由', async () => {
    const snapshot = statusFixture(); snapshot.route.desired = 'rvc';
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(snapshot)));
    const status = await client.status(); expect(status.route).toEqual({ desired: 'rvc', active: 'direct' });
  });
  it('上游拒绝保留 code 与 reason', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(errorResponse('Busy', '队列已满', 409)));
    await expect(client.pause(true)).rejects.toMatchObject({ code: 'Busy', message: '队列已满', httpStatus: 409, resultUnknown: false });
  });
  it('非 JSON 与损坏状态都明确失败', async () => {
    const fetcher = vi.fn().mockResolvedValueOnce(new Response('<html>page</html>', { status: 200 }))
      .mockResolvedValueOnce(jsonResponse({ route: { desired: 'rvc', active: 'pretend' } }));
    vi.stubGlobal('fetch', fetcher);
    await expect(client.status()).rejects.toBeInstanceOf(ApiError);
    await expect(client.status()).rejects.toMatchObject({ code: 'InvalidResponse' });
  });
  it('网络失败的写请求标记未知且绝不重试', async () => {
    const fetcher = vi.fn().mockRejectedValue(new TypeError('connection lost')); vi.stubGlobal('fetch', fetcher);
    await expect(client.up()).rejects.toMatchObject({ resultUnknown: true, code: 'NetworkUnavailable' });
    expect(fetcher).toHaveBeenCalledOnce();
  });
  it('只读查询网络失败不标记写入结果未知', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new TypeError('lost')));
    await expect(client.status()).rejects.toMatchObject({ resultUnknown: false });
  });
  it('配置值使用字符串，保留待重启契约', async () => {
    const fetcher = vi.fn().mockResolvedValue(jsonResponse({ key: 'pre.capture_device', value: 'mic-b', restart_required: true, affected: ['pre'] }));
    vi.stubGlobal('fetch', fetcher);
    const result = await client.setConfig('pre.capture_device', 'mic-b');
    expect(result.restart_required).toBe(true);
    expect(JSON.parse(fetcher.mock.calls[0][1].body)).toEqual({ key: 'pre.capture_device', value: 'mic-b' });
  });
  it('上传字段为 file 与可选 model_id，不设置 multipart 边界', async () => {
    const fetcher = vi.fn().mockResolvedValue(jsonResponse({ job_id: 'j', state: 'queued', queue_position: 1 }, 202)); vi.stubGlobal('fetch', fetcher);
    await client.submit(new File(['audio'], 'sample.wav'), 'missing-model');
    const init = fetcher.mock.calls[0][1];
    expect(init.body.get('file').name).toBe('sample.wav'); expect(init.body.get('model_id')).toBe('missing-model');
    expect(init.body.has('audio_file')).toBe(false); expect(init.headers).toBeUndefined();
  });
  it('下载拒绝不能当成 WAV 保存', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(errorResponse('ResultUnavailable', '结果已驱逐', 409)));
    await expect(downloadResult('job-1')).rejects.toMatchObject({ code: 'ResultUnavailable' });
  });
  it('响应信封不能缺少 data', () => {
    expect(envelopeSchema.safeParse({ status: 'ok', code: 'Ok', reason: '' }).success).toBe(false);
  });
  it('配置响应不能缺少 value', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({})));
    await expect(client.config()).rejects.toMatchObject({ code: 'InvalidResponse' });
  });
  it('HTTP 错误不能包裹成功信封，写入结果仍未知', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse({ state: 'running' }, 503)));
    await expect(client.up()).rejects.toMatchObject({ code: 'InvalidResponse', resultUnknown: true });
  });
  it('读取正文被取消时作为传输错误上报', async () => {
    const response = jsonResponse(statusFixture());
    vi.spyOn(response, 'json').mockRejectedValue(new DOMException('cancelled', 'AbortError'));
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(response));
    await expect(client.status()).rejects.toMatchObject({ code: 'NetworkUnavailable', resultUnknown: false });
  });
  it('下载正文中断必须明确失败', async () => {
    const body = new ReadableStream({ start(controller) { controller.error(new TypeError('socket closed')); } });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(body, { headers: { 'Content-Type': 'audio/wav' } })));
    await expect(downloadResult('job-1')).rejects.toMatchObject({ code: 'NetworkUnavailable', message: 'WAV 下载传输中断' });
  });
  it('下载按 job_id 编码，不接受任意文件路径', async () => {
    const fetcher = vi.fn().mockResolvedValue(new Response('wave', { headers: { 'Content-Type': 'audio/wav', 'Content-Disposition': "attachment; filename*=UTF-8''%E7%BB%93%E6%9E%9C.wav" } }));
    vi.stubGlobal('fetch', fetcher);
    const result = await downloadResult('id/a'); expect(result.name).toBe('结果.wav');
    expect(fetcher.mock.calls[0][0]).toBe('/api/v1/files/id%2Fa/result');
  });
});
