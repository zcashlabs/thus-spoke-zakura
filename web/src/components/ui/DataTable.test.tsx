import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { Link, MemoryRouter, useLocation, useNavigate } from 'react-router-dom';
import { DataTable, Row } from './DataTable';

function History() {
  const navigate = useNavigate();
  return (
    <>
      <output aria-label="Current route">{useLocation().pathname}</output>
      <button onClick={() => void navigate(-1)}>Back</button>
    </>
  );
}

const route = () => screen.getByLabelText('Current route');

function renderTable() {
  render(
    <MemoryRouter initialEntries={['/blocks']}>
      <History />
      <DataTable
        columns={[
          { key: 'block', header: 'Block' },
          { key: 'size', header: 'Size' },
        ]}
      >
        <Row to="/block/7">
          <td>
            <Link to="/block/7">Block 7</Link>
          </td>
          <td>9 KB</td>
        </Row>
      </DataTable>
    </MemoryRouter>,
  );
}

describe('Row navigation', () => {
  it.each([
    ['the link', () => screen.getByRole('link', { name: 'Block 7' })],
    ['another cell', () => screen.getByText('9 KB')],
  ])('navigates once when clicking %s', async (_, target) => {
    renderTable();
    await userEvent.click(target());
    expect(route()).toHaveTextContent('/block/7');
    await userEvent.click(screen.getByRole('button', { name: 'Back' }));
    expect(route()).toHaveTextContent('/blocks');
  });

  it.each(['Meta', 'Control'])('stays on the current page on a %s click', async (key) => {
    const user = userEvent.setup();
    renderTable();
    await user.keyboard(`{${key}>}`);
    await user.click(screen.getByRole('link', { name: 'Block 7' }));
    await user.click(screen.getByText('9 KB'));
    await user.keyboard(`{/${key}}`);
    expect(route()).toHaveTextContent('/blocks');
  });
});
