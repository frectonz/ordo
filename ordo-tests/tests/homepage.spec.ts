import { test, expect } from "@playwright/test";
import { createRoom } from "./utils";

test("has a form", async ({ page }) => {
  await page.goto("/");

  const form = page.getByTestId("create-room-form");

  await expect(form).toBeVisible();
  await expect(form).toHaveAttribute("hx-post", "/rooms");
  await expect(form).toHaveAttribute("hx-ext", "json-enc");
  await expect(form).toHaveAttribute("hx-target", "main");
  await expect(form).toHaveAttribute("hx-swap", "innerHTML");
});

test("adds new options", async ({ page }) => {
  await page.goto("/");

  const button = page.getByText("ADD OPTION");

  expect((await page.getByPlaceholder("a choice").all()).length).toBe(2);
  await button.click();
  expect((await page.getByPlaceholder("a choice").all()).length).toBe(3);
  await button.click();
  expect((await page.getByPlaceholder("a choice").all()).length).toBe(4);
});

test("delete options", async ({ page }) => {
  await page.goto("/");

  const addOption = page.getByText("ADD OPTION");

  expect((await page.getByPlaceholder("a choice").all()).length).toBe(2);
  await addOption.click();
  expect((await page.getByPlaceholder("a choice").all()).length).toBe(3);

  await page.getByText("DELETE").click();

  expect((await page.getByPlaceholder("a choice").all()).length).toBe(2);
});

test("create a new room", async ({ page }) => {
  await page.goto("/");

  expect(await page.title()).toBe("Home - ORDO");

  const name = "best always sunny character";
  const characters = ["Charlie", "Frank", "Sweet Dee", "Dennis", "Mac"];
  await createRoom(page, name, characters);

  await expect(page.getByText(name)).toBeVisible();
  for (const c of characters) {
    await expect(page.getByText(c)).toBeVisible();
  }
  expect(await page.title()).toBe("Admin - ORDO");
});
