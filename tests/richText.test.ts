/**
 * 富文本判定 / 清理纯函数的单测。
 *
 * 仓库没有前端测试框架（无 vitest/jest），这里用 Node 内置 test runner，
 * 零依赖、可直接跑：`pnpm test`（或 `node --test tests/`）。
 * 被测模块 src/utils/richText.ts 不 import 任何东西，因此无需 DOM / Tauri 环境。
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  hasInlineRichText,
  stripInlineRichText,
  hasRichTextFeatures,
} from '../src/utils/richText.ts';

test('hasInlineRichText：label 里的行内语法会被识别', () => {
  assert.equal(hasInlineRichText('![](resource/image/icon/xxx.png) 名称 ★5'), true);
  assert.equal(hasInlineRichText('![三星](icon.png) 名称'), true);
  assert.equal(hasInlineRichText('见 [文档](https://example.com)'), true);
  assert.equal(hasInlineRichText('等级 `max` 说明'), true);
  assert.equal(hasInlineRichText('**重要**选项'), true);
  assert.equal(hasInlineRichText('名称 <b>加粗</b>'), true);
  assert.equal(hasInlineRichText('名称<br>换行'), true);
});

test('hasInlineRichText：纯文本不会被误判（含尖括号的场景）', () => {
  assert.equal(hasInlineRichText('模拟器 <MuMu> 启动'), false);
  assert.equal(hasInlineRichText('使用 <空格> 键暂停'), false);
  assert.equal(hasInlineRichText('攻击力 <100 时撤退'), false);
  assert.equal(hasInlineRichText('总进度 100%'), false);
  assert.equal(hasInlineRichText('名称 ★5 (推荐)'), false);
  assert.equal(hasInlineRichText(''), false);
  assert.equal(hasInlineRichText(undefined), false);
});

test('stripInlineRichText：图片与链接按语义降级', () => {
  assert.equal(stripInlineRichText('![](resource/image/icon/xxx.png) 名称 ★5'), '名称 ★5');
  assert.equal(stripInlineRichText('![三星](icon.png) 名称'), '三星 名称');
  assert.equal(stripInlineRichText('见 [文档](https://example.com) 说明'), '见 文档 说明');
});

test('stripInlineRichText：去掉定界符与标签，保留内容', () => {
  assert.equal(stripInlineRichText('**重要**选项'), '重要选项');
  assert.equal(stripInlineRichText('等级 `max` 说明'), '等级 max 说明');
  assert.equal(stripInlineRichText('名称 <b>加粗</b> 结束'), '名称 加粗 结束');
  assert.equal(stripInlineRichText(' 多   空白\n换行 '), '多 空白 换行');
});

test('stripInlineRichText：白名单之外的尖括号纯文本原样保留（回归）', () => {
  // 曾经的 bug：通用 /<[^>]+>/ 会把纯文本里的 <MuMu> 一起删掉，
  // 使 ComboBox 搜索、任务预览、SwitchGrid title 显示的字凭空变少
  assert.equal(stripInlineRichText('模拟器 <MuMu> 启动'), '模拟器 <MuMu> 启动');
  assert.equal(stripInlineRichText('使用 <空格> 键暂停'), '使用 <空格> 键暂停');
  assert.equal(stripInlineRichText('攻击力 <100 时撤退'), '攻击力 <100 时撤退');
  // 白名单内的行内标签仍然会被去掉
  assert.equal(stripInlineRichText('<br>换行'), '换行');
  assert.equal(stripInlineRichText('名称 <span class="x">强调</span> 结束'), '名称 强调 结束');
});

test('hasRichTextFeatures：日志正文语法面更宽（多行 / 裸 URL）', () => {
  assert.equal(hasRichTextFeatures('第一行\n第二行'), true);
  assert.equal(hasRichTextFeatures('详情见 https://example.com/x'), true);
  assert.equal(hasRichTextFeatures('# 标题'), true);
  assert.equal(hasRichTextFeatures('普通一行文字'), false);
});
