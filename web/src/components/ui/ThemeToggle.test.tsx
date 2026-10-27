import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { ThemeToggle } from './ThemeToggle';

beforeEach(() => localStorage.clear());
afterEach(() => localStorage.clear());

function renderToggle() {
  render(
    <>
      <button type="button">Before</button>
      <ThemeToggle />
      <button type="button">After</button>
    </>,
  );
}

describe('ThemeToggle', () => {
  it('is a single Tab stop on the selected option', async () => {
    renderToggle();
    const user = userEvent.setup();

    await user.click(screen.getByRole('button', { name: 'Before' }));
    await user.tab();
    expect(screen.getByRole('radio', { name: 'System' })).toHaveFocus();
    await user.tab();
    expect(screen.getByRole('button', { name: 'After' })).toHaveFocus();
    await user.tab({ shift: true });
    expect(screen.getByRole('radio', { name: 'System' })).toHaveFocus();
  });

  it('moves focus and selection with the arrow keys and wraps at the ends', async () => {
    renderToggle();
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Before' }));
    await user.tab();

    await user.keyboard('{ArrowRight}');
    expect(screen.getByRole('radio', { name: 'Dark' })).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'Dark' })).toBeChecked();
    await user.keyboard('{ArrowDown}');
    expect(screen.getByRole('radio', { name: 'Light' })).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'Light' })).toBeChecked();
    await user.keyboard('{ArrowLeft}');
    expect(screen.getByRole('radio', { name: 'Dark' })).toBeChecked();
    await user.keyboard('{ArrowUp}');
    expect(screen.getByRole('radio', { name: 'System' })).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'System' })).toBeChecked();
    expect(screen.getAllByRole('radio', { checked: true })).toHaveLength(1);
  });

  it('leaves modified arrow keys to the browser', async () => {
    renderToggle();
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Before' }));
    await user.tab();

    for (const modifier of ['Alt', 'Control', 'Meta']) {
      await user.keyboard(`{${modifier}>}{ArrowLeft}{/${modifier}}`);
      expect(screen.getByRole('radio', { name: 'System' })).toHaveFocus();
      expect(screen.getByRole('radio', { name: 'System' })).toBeChecked();
    }
  });

  it('moves from the focused option', async () => {
    renderToggle();
    const user = userEvent.setup();
    screen.getByRole('radio', { name: 'Light' }).focus();

    await user.keyboard('{ArrowRight}');

    expect(screen.getByRole('radio', { name: 'System' })).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'System' })).toBeChecked();
    await user.keyboard('{ArrowRight}');
    expect(screen.getByRole('radio', { name: 'Dark' })).toBeChecked();
  });

  it('jumps to the first and last option with Home and End', async () => {
    renderToggle();
    const user = userEvent.setup();
    await user.click(screen.getByRole('button', { name: 'Before' }));
    await user.tab();

    await user.keyboard('{End}');
    expect(screen.getByRole('radio', { name: 'Dark' })).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'Dark' })).toBeChecked();
    await user.keyboard('{Home}');
    expect(screen.getByRole('radio', { name: 'Light' })).toHaveFocus();
    expect(screen.getByRole('radio', { name: 'Light' })).toBeChecked();
    await user.tab();
    await user.tab({ shift: true });
    expect(screen.getByRole('radio', { name: 'Light' })).toHaveFocus();
  });
});
