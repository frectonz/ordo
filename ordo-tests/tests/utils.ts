import { Page } from "@playwright/test";

export async function createRoom(page: Page, name: string, choices: string[]) {
  await page.getByPlaceholder("my super cool vote").fill(name);

  const button = page.getByText("ADD OPTION");
  for (let i = 2; i < choices.length; i++) {
    await button.click();
  }

  const options = page.getByPlaceholder("a choice");
  for (let i = 0; i < choices.length; i++) {
    await options.nth(i).fill(choices[i]);
  }

  await page.getByText("CREATE ROOM").click();
  await page.waitForURL("/rooms/*");
}
